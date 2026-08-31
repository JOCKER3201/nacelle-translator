//! Synteza mowy: jeden długożyjący proces pipera z --json-input i --output_dir
//! na tmpfs. Granica wypowiedzi = linia ze ścieżką gotowego WAV na stdout
//! (wzorzec z wyoming-piper). Model ONNX ładuje się raz, na starcie.

use crate::config::TtsCfg;
use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub struct PiperTts {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// częstotliwość próbkowania głosu, z <voice>.onnx.json
    pub sample_rate: u32,
}

impl PiperTts {
    pub fn spawn(cfg: &TtsCfg, piper_bin: &PathBuf, voice: &PathBuf) -> Result<Self> {
        let sample_rate = read_voice_sample_rate(voice)?;
        std::fs::create_dir_all(&cfg.work_dir)
            .with_context(|| format!("nie mogę utworzyć {}", cfg.work_dir))?;
        // po poprzednim crashu (albo padzie pipera między zapisem WAV a jego
        // odczytem) w work_dir mogły zostać osierocone pliki — tmpfs trzyma
        // je w pamięci do rebootu, jeśli nikt ich nie posprząta
        if let Ok(entries) = std::fs::read_dir(&cfg.work_dir) {
            for entry in entries.flatten() {
                if entry.path().extension().is_some_and(|e| e == "wav") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        let mut cmd = Command::new(piper_bin);
        cmd.args([
            "--model",
            voice.to_str().context("ścieżka głosu nie jest UTF-8")?,
            // --config zbędne: piper bierze <model>.json
            // --espeak_data zbędne: binarka ma RUNPATH=$ORIGIN i znajduje dane obok siebie
            "--json-input",
            "--output_dir",
            &cfg.work_dir,
            "--length_scale",
            &cfg.length_scale.to_string(),
            "--sentence_silence",
            &cfg.sentence_silence.to_string(),
            "--quiet",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
        // `main::block_shutdown_signals_before_spawning_threads` blokuje
        // SIGINT/SIGTERM na wątku głównym PRZED tym `spawn()` (żeby wątki
        // toru AI dziedziczyły już zablokowaną maskę — main.rs). fork+exec
        // dziedziczy i ZACHOWUJE maskę sygnałów wołającego wątku (execve nie
        // resetuje maski, tylko dyspozycję sygnałów z handlerem — `man 7
        // signal`), więc bez tego kroku piper startowałby z trwale
        // zablokowanymi SIGINT/SIGTERM i przestałby reagować na `kill`
        // wysłany wprost do jego PID-u albo na Ctrl+C rozgłoszone do grupy
        // procesów (piper dziedziczy pgid rodzica). Odblokowujemy więc oba
        // sygnały W DZIECKU, tuż przed `exec` — działa to niezależnie od
        // maski rodzica, bo `pre_exec` uruchamia się już po `fork`, na
        // wątku, który zaraz i tak zniknie pod `execve`.
        //
        // SAFETY (`pre_exec` samo jest `unsafe fn` — dziecko po `fork`,
        // przed `exec`): closure woła wyłącznie `sigemptyset`/`sigaddset`/
        // `pthread_sigmask` na lokalnym, stosowym `sigset_t` — nic poza
        // async-signal-safe, zgodnie z wymogiem `pre_exec`.
        unsafe {
            cmd.pre_exec(|| {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGINT);
                libc::sigaddset(&mut set, libc::SIGTERM);
                libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("nie mogę uruchomić pipera: {}", piper_bin.display()))?;

        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Ok(Self {
            child,
            stdin,
            stdout,
            sample_rate,
        })
    }

    /// Blokuje do końca syntezy; zwraca próbki f32 mono w `sample_rate`.
    pub fn synthesize(&mut self, text: &str) -> Result<Vec<f32>> {
        let line = serde_json::json!({ "text": text }).to_string();
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;

        let mut path_line = String::new();
        if self.stdout.read_line(&mut path_line)? == 0 {
            bail!("piper zakończył się nieoczekiwanie (EOF na stdout)");
        }
        let wav_path = path_line.trim_end();

        // Sprzątanie MUSI się wykonać niezależnie od wyniku parsowania —
        // inaczej uszkodzony/ucięty WAV (błąd `?` w środku) zostaje w
        // /dev/shm na zawsze (tmpfs = pamięć RAM, kumuluje się do rebootu).
        let result = (|| -> Result<Vec<f32>> {
            let mut reader = hound::WavReader::open(wav_path)
                .with_context(|| format!("nie mogę otworzyć WAV pipera: {wav_path}"))?;
            let spec = reader.spec();
            let samples: Vec<f32> = match spec.sample_format {
                hound::SampleFormat::Int => reader
                    .samples::<i16>()
                    .map(|s| s.map(|v| v as f32 / 32768.0))
                    .collect::<std::result::Result<_, _>>()?,
                hound::SampleFormat::Float => reader
                    .samples::<f32>()
                    .collect::<std::result::Result<_, _>>()?,
            };
            Ok(samples)
        })();
        let _ = std::fs::remove_file(wav_path);
        result
    }
}

impl Drop for PiperTts {
    fn drop(&mut self) {
        // zamknięcie stdin kończy pipera; kill jako zabezpieczenie
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Czyta "audio.sample_rate" z <voice>.onnx.json.
pub fn read_voice_sample_rate(voice: &PathBuf) -> Result<u32> {
    let json_path = PathBuf::from(format!("{}.json", voice.display()));
    let raw = std::fs::read_to_string(&json_path)
        .with_context(|| format!("brak configu głosu: {}", json_path.display()))?;
    let v: serde_json::Value = serde_json::from_str(&raw)?;
    v["audio"]["sample_rate"]
        .as_u64()
        .map(|r| r as u32)
        .context("brak audio.sample_rate w configu głosu")
}
