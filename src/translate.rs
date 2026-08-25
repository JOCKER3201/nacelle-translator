//! Tłumaczenie: Gemini Flash (domyślnie, API w chmurze — czas rzeczywisty),
//! lokalna Ollama, albo API Claude — wszystkie przez surowe HTTP. Silnik
//! "ollama" nie wymaga żadnego klucza (serwer lokalny, bez
//! uwierzytelniania); "gemini" czyta klucz z GEMINI_API_KEY; "claude" —
//! z ANTHROPIC_API_KEY.

use crate::config::MtCfg;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::time::Duration;

const CLAUDE_API_URL: &str = "https://api.anthropic.com/v1/messages";
const CLAUDE_API_VERSION: &str = "2023-06-01";
const GEMINI_API_URL: &str = "https://generativelanguage.googleapis.com/v1beta/interactions";
/// powyżej tego czasu nie czekamy inline na retry-after — kanały mt/tts są
/// bounded(1), więc długi sen w tym wątku cofa backpressure aż do
/// segmentera; lepiej pominąć fragment niż zablokować cały tor na dłużej
const MAX_INLINE_RETRY_WAIT: Duration = Duration::from_secs(8);
/// jak długo Ollama ma trzymać model w pamięci między wypowiedziami —
/// model rzędu kilkunastu-kilkudziesięciu GB potrafi ładować się dziesiątki
/// sekund; bez tego pierwsza wypowiedź po każdej dłuższej ciszy znów płaci
/// pełny cold-start
const OLLAMA_KEEP_ALIVE: &str = "30m";

pub trait Translator: Send {
    fn translate(&mut self, text: &str, src_lang: &str) -> Result<String>;
}

/// Tryb "off": tekst przechodzi bez tłumaczenia (test toru audio).
pub struct Passthrough;

impl Translator for Passthrough {
    fn translate(&mut self, text: &str, _src_lang: &str) -> Result<String> {
        Ok(text.to_string())
    }
}

fn system_prompt(target_language: &str) -> String {
    format!(
        "Jesteś profesjonalnym tłumaczem symultanicznym. Tłumaczysz kolejne, \
         krótkie fragmenty ścieżki dźwiękowej (film, rozmowa, transmisja) na język: {target_language}. \
         Zasady: zwracasz WYŁĄCZNIE tłumaczenie — bez komentarzy, cudzysłowów i objaśnień. \
         Zachowujesz ton i rejestr wypowiedzi. Utrzymujesz spójność imion i terminów \
         z poprzednimi fragmentami. Liczby, daty i skróty zapisujesz słownie, tak jak \
         przeczytałby je lektor. Fragment może być urwany w pół zdania — tłumaczysz go \
         tak, jak brzmi, niczego nie dopowiadasz."
    )
}

// ================= Gemini Flash (API, domyślny) =================
//
// Google w czerwcu 2026 zastąpił dotychczasowe `generateContent` nowym
// Interactions API (POST /v1beta/interactions) — model idzie w ciele JSON
// (nie w ścieżce URL), autoryzacja przez nagłówek x-goog-api-key, a
// wieloturowa rozmowa jest STANOWA po stronie serwera: zamiast za każdym
// razem przesyłać całą historię, przekazuje się `previous_interaction_id`
// zwrócone z poprzedniej odpowiedzi. Żeby kontekst nie rósł bez końca przez
// całą sesję nasłuchu, łańcuch jest resetowany co `context_pairs` tur —
// to jedyny sposób na ograniczenie okna bez ręcznego przepisywania historii
// (dokumentacja API nie opisuje jawnie formatu tablicy Step/Content do
// odtwarzania historii w trybie bezstanowym).
pub struct GeminiTranslator {
    cfg: MtCfg,
    api_key: String,
    agent: ureq::Agent,
    system_prompt: String,
    previous_interaction_id: Option<String>,
    turns_since_reset: usize,
}

impl GeminiTranslator {
    pub fn new(cfg: MtCfg) -> Result<Self> {
        let api_key = std::env::var("GEMINI_API_KEY")
            .context("brak zmiennej środowiskowej GEMINI_API_KEY")?;
        if api_key.trim().is_empty() {
            bail!("zmienna GEMINI_API_KEY jest pusta");
        }
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(cfg.timeout_s))
            .build();
        Ok(Self {
            system_prompt: system_prompt(&cfg.target_language),
            cfg,
            api_key,
            agent,
            previous_interaction_id: None,
            turns_since_reset: 0,
        })
    }

    fn call_once(&self, body: &Value) -> std::result::Result<Value, ureq::Error> {
        self.agent
            .post(GEMINI_API_URL)
            .set("x-goog-api-key", &self.api_key)
            .set("content-type", "application/json")
            .send_json(body.clone())?
            .into_json::<Value>()
            .map_err(ureq::Error::from)
    }

    fn call_with_retry(&self, body: &Value) -> Result<Value> {
        match self.call_once(body) {
            Ok(v) => Ok(v),
            Err(ureq::Error::Status(code, resp)) if code == 429 || code >= 500 => {
                let wait_s = resp.header("retry-after").and_then(|s| s.parse::<u64>().ok());
                let wait = wait_s
                    .map(Duration::from_secs)
                    .filter(|w| *w <= MAX_INLINE_RETRY_WAIT)
                    .unwrap_or(Duration::from_secs(5));
                log::warn!("Gemini: HTTP {code}, ponawiam za {}s", wait.as_secs());
                std::thread::sleep(wait);
                self.call_once(body)
                    .map_err(|e| anyhow!("Gemini po ponowieniu: {e}"))
            }
            Err(e) => Err(anyhow!("Gemini: {e}")),
        }
    }
}

impl Translator for GeminiTranslator {
    fn translate(&mut self, text: &str, _src_lang: &str) -> Result<String> {
        let continuing = self.previous_interaction_id.is_some()
            && self.turns_since_reset < self.cfg.context_pairs;

        let mut body = json!({
            "model": self.cfg.gemini_model,
            "input": text,
            "system_instruction": self.system_prompt,
            "generation_config": { "max_output_tokens": self.cfg.max_tokens },
        });
        if continuing {
            body["previous_interaction_id"] = json!(self.previous_interaction_id);
        }

        let resp = self.call_with_retry(&body)?;

        let translated: String = resp["steps"]
            .as_array()
            .map(|steps| {
                steps
                    .iter()
                    .filter(|s| s["type"].as_str() == Some("model_output"))
                    .filter_map(|s| s["content"].as_array())
                    .flatten()
                    .filter(|c| c["type"].as_str() == Some("text"))
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default()
            .trim()
            .to_string();
        if translated.is_empty() {
            bail!("Gemini: pusta odpowiedź (status: {})", resp["status"]);
        }

        if let Some(id) = resp["id"].as_str() {
            self.previous_interaction_id = Some(id.to_string());
            self.turns_since_reset = if continuing { self.turns_since_reset + 1 } else { 1 };
        } else {
            // brak id w odpowiedzi — nie da się kontynuować łańcucha,
            // kolejne wywołanie zacznie świeżą interakcję
            self.previous_interaction_id = None;
        }
        Ok(translated)
    }
}

// ================= Ollama (lokalny model) =================

pub struct OllamaTranslator {
    cfg: MtCfg,
    agent: ureq::Agent,
    system_prompt: String,
    history: VecDeque<(String, String)>,
}

/// Sprawdza samo połączenie z serwerem i obecność modelu (bez ładowania go
/// do pamięci) — używane zarówno przy starcie translatora, jak i przez
/// `nacelle-translator check`, gdzie pełna rozgrzewka (dziesiątki sekund dla
/// dużego modelu) byłaby zbyt kosztowna na szybką diagnostykę.
pub fn ollama_check(host: &str, model: &str) -> Result<()> {
    let tags_url = format!("{}/api/tags", host.trim_end_matches('/'));
    let resp: Value = ureq::get(&tags_url)
        .timeout(Duration::from_secs(5))
        .call()
        .with_context(|| {
            format!(
                "nie mogę połączyć się z Ollamą pod {host} — czy usługa działa? \
                 (systemctl status ollama)"
            )
        })?
        .into_json()
        .context("Ollama: niepoprawna odpowiedź /api/tags")?;
    let known_models: Vec<String> = resp["models"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let has_model = known_models
        .iter()
        .any(|n| n == model || n.starts_with(&format!("{model}:")));
    if !has_model {
        bail!(
            "Ollama nie ma modelu \"{model}\" (dostępne: {}) — pobierz go: ollama pull {model}",
            if known_models.is_empty() {
                "brak żadnych modeli".to_string()
            } else {
                known_models.join(", ")
            }
        );
    }
    Ok(())
}

impl OllamaTranslator {
    pub fn new(cfg: MtCfg) -> Result<Self> {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(cfg.ollama_timeout_s))
            .build();

        // fail-fast: serwer nieosiągalny albo model nieściągnięty — lepiej
        // dowiedzieć się na starcie niż przy pierwszej wypowiedzi w torze na żywo
        ollama_check(&cfg.ollama_host, &cfg.ollama_model)?;

        let translator = Self {
            system_prompt: system_prompt(&cfg.target_language),
            cfg,
            agent,
            history: VecDeque::new(),
        };
        // rozgrzewka: wczytuje wagi modelu do pamięci teraz, żeby pierwsza
        // prawdziwa wypowiedź w torze na żywo nie płaciła cold-startu
        // (dla modeli rzędu kilkunastu-kilkudziesięciu GB to bywa >10 s)
        if let Err(e) = translator.call_chat(&translator.build_messages("witaj")) {
            log::warn!("Ollama: rozgrzewka modelu nie powiodła się (kontynuuję): {e:#}");
        } else {
            log::info!("Ollama: model \"{}\" załadowany i gotowy", translator.cfg.ollama_model);
        }
        Ok(translator)
    }

    fn build_messages(&self, text: &str) -> Vec<Value> {
        let mut messages = vec![json!({ "role": "system", "content": self.system_prompt })];
        for (src, dst) in &self.history {
            messages.push(json!({ "role": "user", "content": src }));
            messages.push(json!({ "role": "assistant", "content": dst }));
        }
        messages.push(json!({ "role": "user", "content": text }));
        messages
    }

    fn call_chat(&self, messages: &[Value]) -> Result<Value> {
        let url = format!("{}/api/chat", self.cfg.ollama_host.trim_end_matches('/'));
        let body = json!({
            "model": self.cfg.ollama_model,
            "messages": messages,
            "stream": false,
            "keep_alive": OLLAMA_KEEP_ALIVE,
            "options": { "num_predict": self.cfg.max_tokens },
        });
        match self.agent.post(&url).send_json(body.clone()) {
            Ok(resp) => resp.into_json::<Value>().map_err(|e| anyhow!("Ollama: {e}")),
            // pojedyncze ponowienie — bez retry-after (serwer lokalny, brak rate-limitów;
            // 500 zwykle znaczy chwilową kolizję z ładowaniem/wyładowywaniem modelu)
            Err(ureq::Error::Status(code, _)) if code >= 500 => {
                log::warn!("Ollama: HTTP {code}, ponawiam za 3s");
                std::thread::sleep(Duration::from_secs(3));
                self.agent
                    .post(&url)
                    .send_json(body)
                    .map_err(|e| anyhow!("Ollama po ponowieniu: {e}"))?
                    .into_json::<Value>()
                    .map_err(|e| anyhow!("Ollama: {e}"))
            }
            Err(e) => Err(anyhow!("Ollama: {e}")),
        }
    }
}

impl Translator for OllamaTranslator {
    fn translate(&mut self, text: &str, _src_lang: &str) -> Result<String> {
        let messages = self.build_messages(text);
        let resp = self.call_chat(&messages)?;

        let translated = resp["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        if translated.is_empty() {
            bail!("Ollama: pusta odpowiedź (done_reason: {})", resp["done_reason"]);
        }

        self.history.push_back((text.to_string(), translated.clone()));
        while self.history.len() > self.cfg.context_pairs {
            self.history.pop_front();
        }
        Ok(translated)
    }
}

// ================= llama.cpp (llama-server, lokalny model GGUF na GPU) =================
//
// Silnik szyty pod TranslateGemma: wbudowany szablon czatu tego modelu
// (tokenizer.chat_template w GGUF) wymaga content[] ze strukturą
// {type, source_lang_code, target_lang_code, text} i wywraca automatyczny
// generator parserów llama-server już przy starcie — dlatego serwer
// uruchamia się z --no-jinja, a prompt składamy tutaj sami, odtwarzając
// szablon 1:1, i wysyłamy surowym endpointem /completion (bez szablonów
// po stronie serwera). Historia par wypowiedzi idzie w prompcie jako
// naprzemienne tury user/model, tak jak renderowałby ją oryginalny szablon.

pub struct LlamaCppTranslator {
    cfg: MtCfg,
    agent: ureq::Agent,
    /// (kod języka źródłowego, oryginał, tłumaczenie) — kod per wpis,
    /// bo autodetekcja whispera może dać inny język dla każdej wypowiedzi
    history: VecDeque<(String, String, String)>,
}

/// Angielska nazwa języka do promptu TranslateGemma ("en" → "English").
/// Kody i nazwy pochodzą z tablicy whispera — dokładnie tego zbioru używa
/// nasza detekcja języka, więc każdy kod, który tu trafia, ma nazwę.
fn english_lang_name(code: &str) -> Option<String> {
    let id = whisper_rs::get_lang_id(code)?;
    let full = whisper_rs::get_lang_str_full(id)?;
    let mut chars = full.chars();
    let first = chars.next()?;
    Some(first.to_uppercase().collect::<String>() + chars.as_str())
}

/// Sprawdza tylko dostępność serwera (endpoint /health) — bez wysyłania
/// promptu, żeby szybko wykryć brak uruchomionego llama-server zamiast
/// czekać na timeout pierwszego prawdziwego tłumaczenia w torze na żywo.
pub fn llamacpp_check(host: &str) -> Result<()> {
    let url = format!("{}/health", host.trim_end_matches('/'));
    ureq::get(&url)
        .timeout(Duration::from_secs(5))
        .call()
        .with_context(|| {
            format!(
                "nie mogę połączyć się z llama-server pod {host} — czy serwer działa? \
                 (llama-server -m <model.gguf> --port 8080 --no-jinja)"
            )
        })?;
    Ok(())
}

impl LlamaCppTranslator {
    pub fn new(cfg: MtCfg) -> Result<Self> {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(cfg.timeout_s))
            .build();
        llamacpp_check(&cfg.llamacpp_host)?;
        Ok(Self {
            cfg,
            agent,
            history: VecDeque::new(),
        })
    }

    /// Jedna tura użytkownika w formacie szablonu TranslateGemma —
    /// odwzorowanie fragmentu `content["type"] == 'text'` z
    /// tokenizer.chat_template (łącznie z potrójnym \n przed tekstem).
    fn user_turn(&self, src_code: &str, text: &str) -> String {
        let src_name =
            english_lang_name(src_code).unwrap_or_else(|| "English".into());
        let tgt_code = &self.cfg.target_lang_code;
        let tgt_name =
            english_lang_name(tgt_code).unwrap_or_else(|| self.cfg.target_language.clone());
        format!(
            "<start_of_turn>user\n\
             You are a professional {src_name} ({src_code}) to {tgt_name} ({tgt_code}) \
             translator. Your goal is to accurately convey the meaning and nuances of the \
             original {src_name} text while adhering to {tgt_name} grammar, vocabulary, \
             and cultural sensitivities.\n\
             Produce only the {tgt_name} translation, without any additional explanations \
             or commentary. Please translate the following {src_name} text into \
             {tgt_name}:\n\n\n{}<end_of_turn>\n",
            text.trim()
        )
    }

    fn build_prompt(&self, src_code: &str, text: &str) -> String {
        let mut p = String::new();
        for (hist_src, hist_text, hist_dst) in &self.history {
            p.push_str(&self.user_turn(hist_src, hist_text));
            p.push_str("<start_of_turn>model\n");
            p.push_str(hist_dst.trim());
            p.push_str("<end_of_turn>\n");
        }
        p.push_str(&self.user_turn(src_code, text));
        p.push_str("<start_of_turn>model\n");
        p
    }

    fn call_once(&self, body: &Value) -> std::result::Result<Value, ureq::Error> {
        let url = format!("{}/completion", self.cfg.llamacpp_host.trim_end_matches('/'));
        self.agent
            .post(&url)
            .set("content-type", "application/json")
            .send_json(body.clone())?
            .into_json::<Value>()
            .map_err(ureq::Error::from)
    }

    fn call_with_retry(&self, body: &Value) -> Result<Value> {
        match self.call_once(body) {
            Ok(v) => Ok(v),
            // serwer lokalny bez rate-limitów; 500 zwykle znaczy chwilową
            // kolizję (np. jeszcze się rozgrzewa) — jedno ponowienie
            Err(ureq::Error::Status(code, _)) if code >= 500 => {
                log::warn!("llama-server: HTTP {code}, ponawiam za 3s");
                std::thread::sleep(Duration::from_secs(3));
                self.call_once(body)
                    .map_err(|e| anyhow!("llama-server po ponowieniu: {e}"))
            }
            Err(e) => Err(anyhow!("llama-server: {e}")),
        }
    }
}

impl Translator for LlamaCppTranslator {
    fn translate(&mut self, text: &str, src_lang: &str) -> Result<String> {
        // szablon wymaga konkretnego kodu języka źródłowego; przy nieudanej
        // detekcji whispera ("und"/nieznany kod) bierzemy angielski jako
        // najbezpieczniejszy domysł zamiast zrywać tłumaczenie
        let src_code = if english_lang_name(src_lang).is_some() {
            src_lang
        } else {
            log::warn!("llama-server: nieznany kod języka \"{src_lang}\", zakładam \"en\"");
            "en"
        };

        let body = json!({
            "prompt": self.build_prompt(src_code, text),
            "n_predict": self.cfg.max_tokens,
            "temperature": 0.0,
            "stream": false,
            "cache_prompt": true,
            "stop": ["<end_of_turn>"],
        });
        let resp = self.call_with_retry(&body)?;

        let translated = resp["content"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        if translated.is_empty() {
            bail!(
                "llama-server: pusta odpowiedź (stop_type: {})",
                resp["stop_type"]
            );
        }

        self.history
            .push_back((src_code.to_string(), text.to_string(), translated.clone()));
        while self.history.len() > self.cfg.context_pairs {
            self.history.pop_front();
        }
        Ok(translated)
    }
}

// ================= Claude (API) =================

pub struct ClaudeTranslator {
    cfg: MtCfg,
    api_key: String,
    agent: ureq::Agent,
    system_prompt: String,
    /// przesuwne okno par (oryginał, tłumaczenie) — spójność terminologii
    history: VecDeque<(String, String)>,
}

impl ClaudeTranslator {
    pub fn new(cfg: MtCfg) -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .context("brak zmiennej środowiskowej ANTHROPIC_API_KEY")?;
        if api_key.trim().is_empty() {
            bail!("zmienna ANTHROPIC_API_KEY jest pusta");
        }
        // Agent trzyma pulę połączeń keep-alive — bez niego każdy fragment
        // (co kilka sekund, w torze na żywo) płaciłby pełny handshake
        // TCP+TLS do api.anthropic.com.
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(cfg.timeout_s))
            .build();
        Ok(Self {
            system_prompt: system_prompt(&cfg.target_language),
            cfg,
            api_key,
            agent,
            history: VecDeque::new(),
        })
    }

    fn build_body(&self, text: &str) -> Value {
        let mut messages = Vec::new();
        for (src, dst) in &self.history {
            messages.push(json!({ "role": "user", "content": src }));
            messages.push(json!({ "role": "assistant", "content": dst }));
        }
        messages.push(json!({ "role": "user", "content": text }));
        json!({
            "model": self.cfg.model,
            "max_tokens": self.cfg.max_tokens,
            "system": self.system_prompt,
            "messages": messages,
        })
    }

    fn call_once(&self, body: &Value) -> std::result::Result<Value, ureq::Error> {
        self.agent
            .post(CLAUDE_API_URL)
            .set("x-api-key", &self.api_key)
            .set("anthropic-version", CLAUDE_API_VERSION)
            .set("content-type", "application/json")
            .send_json(body.clone())?
            .into_json::<Value>()
            .map_err(ureq::Error::from)
    }

    fn call_with_retry(&self, body: &Value) -> Result<Value> {
        match self.call_once(body) {
            Ok(v) => Ok(v),
            Err(ureq::Error::Status(code, resp)) if code == 429 || code >= 500 => {
                let wait_s = resp.header("retry-after").and_then(|s| s.parse::<u64>().ok());
                match wait_s {
                    Some(w) if Duration::from_secs(w) <= MAX_INLINE_RETRY_WAIT => {
                        log::warn!("API Claude: HTTP {code}, ponawiam za {w}s");
                        std::thread::sleep(Duration::from_secs(w));
                        self.call_once(body)
                            .map_err(|e| anyhow!("API Claude po ponowieniu: {e}"))
                    }
                    Some(w) => {
                        bail!(
                            "API Claude: HTTP {code}, zalecany limit {w}s przekracza budżet \
                             ponowienia ({}s) — pomijam fragment zamiast blokować tor",
                            MAX_INLINE_RETRY_WAIT.as_secs()
                        )
                    }
                    None => {
                        // brak nagłówka albo nieparsowalny (np. data HTTP zamiast liczby
                        // sekund) — krótkie, ograniczone ponowienie
                        log::warn!("API Claude: HTTP {code} bez czytelnego retry-after, ponawiam za 5s");
                        std::thread::sleep(Duration::from_secs(5));
                        self.call_once(body)
                            .map_err(|e| anyhow!("API Claude po ponowieniu: {e}"))
                    }
                }
            }
            Err(e) => Err(anyhow!("API Claude: {e}")),
        }
    }
}

impl Translator for ClaudeTranslator {
    fn translate(&mut self, text: &str, _src_lang: &str) -> Result<String> {
        let body = self.build_body(text);
        let resp = self.call_with_retry(&body)?;

        let stop_reason = resp["stop_reason"].as_str().unwrap_or_default();
        if stop_reason == "refusal" {
            bail!("API Claude odmówiło przetworzenia fragmentu (refusal)");
        }
        let translated: String = resp["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b["type"].as_str() == Some("text"))
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default()
            .trim()
            .to_string();
        if translated.is_empty() {
            bail!("API Claude: pusta odpowiedź ({stop_reason})");
        }

        if stop_reason == "max_tokens" {
            // Odpowiedź jest ucięta w pół zdania — nadal warto ją przeczytać
            // (lepsze urwane tłumaczenie niż cisza), ale NIE wolno wstawiać
            // jej do historii: uczyłoby to model, że urywanie zdań jest OK.
            log::warn!(
                "API Claude: tłumaczenie ucięte (max_tokens={}), pomijam wpis w historii",
                self.cfg.max_tokens
            );
            return Ok(translated);
        }

        self.history.push_back((text.to_string(), translated.clone()));
        while self.history.len() > self.cfg.context_pairs {
            self.history.pop_front();
        }
        Ok(translated)
    }
}

pub fn make_translator(cfg: &MtCfg) -> Result<Box<dyn Translator>> {
    match cfg.engine.as_str() {
        "gemini" => Ok(Box::new(GeminiTranslator::new(cfg.clone())?)),
        "ollama" => Ok(Box::new(OllamaTranslator::new(cfg.clone())?)),
        "llamacpp" => Ok(Box::new(LlamaCppTranslator::new(cfg.clone())?)),
        "claude" => Ok(Box::new(ClaudeTranslator::new(cfg.clone())?)),
        "off" => Ok(Box::new(Passthrough)),
        other => bail!(
            "nieznany silnik tłumaczenia: {other} (dozwolone: gemini, ollama, llamacpp, claude, off)"
        ),
    }
}
