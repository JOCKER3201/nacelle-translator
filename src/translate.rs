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

/// Zwija warianty rodzajowe zapisane ukośnikiem ("pewien/pewna") do
/// PIERWSZEGO wariantu. Piper fonemizuje przez espeak-ng, a ten czyta '/'
/// między literami jako angielskie "slash" — zweryfikowane fonemizatorem:
/// "Nie jestem pewien/pewna w tej chwili." daje segment sl'ES, czyli lektor
/// dosłownie wypowiedział "pewien SLASZ pewna". Skala: 1 wystąpienie na 180
/// wyjść MT w sesji audytu.
///
/// Warunki zwinięcia (WSZYSTKIE muszą być spełnione) — dobrane tak, żeby
/// legalne zapisy techniczne przechodziły NIETKNIĘTE:
///  * po obu stronach ukośnika wyłącznie litery (`char::is_alphabetic`),
///  * oba biegi są całymi słowami: znak przed lewym i po prawym nie jest
///    alfanumeryczny (odsiewa "2.5Gb/s", "5GB/GBps"),
///  * wspólny prefiks >= 3 ZNAKÓW (nie bajtów!), bez rozróżniania wielkości
///    liter — odsiewa "Gb/s", "WAN/LAN", "km/h", "wejście/wyjście" (prefiks 1),
///  * długości wariantów różnią się najwyżej o 2 znaki,
///  * ŻADEN bieg nie jest prefiksem drugiego — odsiewa akronim i jego
///    rozszerzenie ("HTTP/HTTPS", "USB/USBC", "PoE/PoE+"), które oba warunki
///    wyżej przechodzą, a zwinięcie kosztowałoby połowę treści merytorycznej.
///
/// Znany, ZAAKCEPTOWANY fałszywy alarm: "przed/przez" (wspólny prefiks
/// "prze"). Przy jednym ukośniku na całą 7-minutową sesję to koszt pomijalny.
///
/// NIE rozszerzać na cudzysłowy ani nawiasy: zweryfikowane espeakiem, że
/// cudzysłowy (proste i drukarskie) NIE dają ŻADNYCH fonemów — espeak
/// traktuje je jak przecinek. Taki kod byłby czystym długiem bez efektu.
fn collapse_variant_slashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    // przesunięcie w WYJŚCIU, od którego ciągną się litery bieżącego słowa;
    // None = nie ma otwartego słowa albo ciąg liter zaczął się po cyfrze
    // (wtedy nie jest CAŁYM słowem, więc "2.5Gb/s" się nie kwalifikuje)
    let mut word_start: Option<usize> = None;
    let mut prev_alnum = false;

    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if c == '/' {
            let rest = &s[i + c.len_utf8()..];
            let right_len = rest
                .find(|ch: char| !ch.is_alphabetic())
                .unwrap_or(rest.len());
            // prawy bieg też musi kończyć słowo — inaczej "5GB/GBps"
            let boundary_ok = rest[right_len..]
                .chars()
                .next()
                .map_or(true, |ch| !ch.is_alphanumeric());
            let collapse = right_len > 0
                && boundary_ok
                && word_start.is_some_and(|w| same_variant(&out[w..], &rest[..right_len]));
            if collapse {
                // zjedz prawy wariant razem z ukośnikiem; iterujemy po ZNAKACH,
                // bo polskie diakrytyki mają po 2 bajty
                for _ in 0..rest[..right_len].chars().count() {
                    it.next();
                }
            } else {
                out.push(c);
            }
            word_start = None;
            prev_alnum = false;
            continue;
        }
        if c.is_alphabetic() {
            if word_start.is_none() && !prev_alnum {
                word_start = Some(out.len());
            }
            prev_alnum = true;
        } else {
            word_start = None;
            prev_alnum = c.is_alphanumeric();
        }
        out.push(c);
    }
    out
}

/// Czy `left` i `right` to dwa warianty fleksyjne tego samego słowa.
/// Wszystko liczone w ZNAKACH, nie bajtach — "żółty/żółta" ma 5 znaków,
/// ale 7 bajtów, a cięcie po bajtach panikowałoby na granicy diakrytyku.
fn same_variant(left: &str, right: &str) -> bool {
    let (ln, rn) = (left.chars().count(), right.chars().count());
    if ln.abs_diff(rn) > 2 {
        return false;
    }
    let common = left
        .chars()
        .flat_map(char::to_lowercase)
        .zip(right.chars().flat_map(char::to_lowercase))
        .take_while(|(a, b)| a == b)
        .count();
    // Wariant fleksyjny różni się KOŃCÓWKĄ, więc żaden bieg nie jest prefiksem
    // drugiego ("pewien"/"pewna" rozjeżdżają się na 4. znaku). Akronim i jego
    // rozszerzenie są dokładnie odwrotne: "HTTP"/"HTTPS", "USB"/"USBC",
    // "PoE"/"PoE+" mają wspólny prefiks równy krótszemu biegowi i sam warunek
    // >= 3 znaków ich NIE odsiewa — a materiał, na którym ten sanitizer ma
    // działać, to recenzja sprzętu sieciowego, gdzie taka para jest treścią
    // merytoryczną, nie ozdobnikiem. Zwinięcie usunęłoby połowę informacji.
    common >= 3 && common < ln.min(rn)
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

    /// Polityka kontekstu przed złożeniem promptu.
    ///
    /// Prefiks promptu musi być append-only między resetami, żeby cache_prompt
    /// llama-servera trafiał (rolowanie okna przez pop_front co turę zmieniałoby
    /// prefiks przy KAŻDEJ turze i wymuszało pełny reprocessing). Ale twardy
    /// clear() płacił za to zbyt drogo: pomiar audytu na 180 turach dał rozkład
    /// głębokości historii 60/60/60 przy hist_len 0/1/2 — DOKŁADNIE 1/3
    /// tłumaczeń szła z pustym kontekstem, i to na tych turach wypadły oba
    /// najcięższe błędy znaczeniowe sesji ("a crapload of ventilation" ->
    /// "Bardzo słabe wentylacja"; poprzednia tura kończyła się na "...robust
    /// built device with a", czyli zawierała jedyny dysambiguator idiomu,
    /// i została wyrzucona 100 ms wcześniej). Z 10 skatalogowanych wpadek MT:
    /// 5 przy hist_len=0, 5 przy hist_len=1, ZERO przy hist_len=2.
    ///
    /// Dlatego przy resecie zostawiamy OSTATNIĄ parę. Musi zostać na PRZODZIE
    /// kolejki, przed pierwszą nową turą — inaczej prefiks przestaje być
    /// append-only i cache pada całkowicie. Dowód, że po tej zmianie prefiks
    /// nadal tylko rośnie MIĘDZY resetami: build_prompt składa historię
    /// w kolejności kolejki, a po udanej turze dokładamy jej parę przez
    /// push_back z DOKŁADNIE tym samym (src_code, text), z których zbudowano
    /// prompt — więc prompt tury n+1 to prompt tury n plus doklejone
    /// "<odpowiedź n><end_of_turn>\n" i nowa tura user. Turę, w której
    /// pop_front faktycznie zadziała, płacimy jednym chybieniem cache
    /// (mediana MT 76 ms, suma 16 s na 7 minut). Zapas jest: kontekst nigdy
    /// nie przekroczył 338 tokenów przy n_ctx_slot=131072.
    ///
    /// Degeneraty context_pairs 0 i 1 zachowują dotychczasowe zachowanie
    /// (pusta historia) — przy jednej parze nie ma "ostatniej pary"
    /// do zachowania.
    fn trim_history(&mut self) {
        let keep = usize::from(self.cfg.context_pairs >= 2);
        if self.history.len() >= self.cfg.context_pairs {
            while self.history.len() > keep {
                self.history.pop_front();
            }
        }
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

        self.trim_history();

        // n_predict proporcjonalny do wejścia zamiast globalnego max_tokens:
        // na zniekształconej transkrypcji model potrafi halucynować rozdęte,
        // wielokrotnie dłuższe "tłumaczenie" (obserwowane: 2s oryginału →
        // 7.7s lektora), które zapycha kolejkę odtwarzania na wiele sekund.
        // 4 tokeny na słowo wejścia + zapas to dużo nawet dla fleksyjnej
        // polszczyzny — legalne tłumaczenia się mieszczą, rozdęcia są ucinane.
        let input_words = text.split_whitespace().count() as u32;
        let n_predict = (input_words * 4 + 24).min(self.cfg.max_tokens);
        let body = json!({
            "prompt": self.build_prompt(src_code, text),
            "n_predict": n_predict,
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

        // sanityzacja przed TTS i przed wpisem do historii — jedno źródło
        // prawdy: model widzi w kontekście dokładnie to, co usłyszał słuchacz
        let translated = collapse_variant_slashes(&translated);

        self.history
            .push_back((src_code.to_string(), text.to_string(), translated.clone()));
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

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- rekomendacja 9: sanitizer ukośnika ----------

    // TR1: wzorzec, który faktycznie wystąpił w logu (#63.0) — espeak-ng
    // fonemizował '/' jako sl'ES, więc lektor powiedział "pewien SLASZ pewna"
    #[test]
    fn tr1_ukosnik_wariantu_rodzajowego_zwiniety() {
        assert_eq!(
            collapse_variant_slashes("Nie jestem pewien/pewna w tej chwili."),
            "Nie jestem pewien w tej chwili."
        );
    }

    // TR2: zapisy techniczne z materiału audytu MUSZĄ przejść nietknięte —
    // to one decydują o tym, że sanitizer jest bezpieczny na tej domenie
    #[test]
    fn tr2_zapisy_techniczne_nietkniete() {
        for s in [
            "2.5 Gb/s",
            "WAN/LAN",
            "24/7",
            "km/h",
            "2.5Gb/s",
            "wejście/wyjście",
            "AdGuard/OpenWRT",
            "Wi-Fi 7 / MLO",
            "5GB/GBps",
            // akronim + rozszerzenie: różnica długości <= 2 i wspólny prefiks
            // >= 3 SPEŁNIONE, odsiewa je dopiero warunek "żaden bieg nie jest
            // prefiksem drugiego". Bez niego "HTTP/HTTPS" czytało się "HTTP".
            "HTTP/HTTPS",
            "USB/USBC",
            "PoE/PoE+",
        ] {
            assert_eq!(collapse_variant_slashes(s), s, "zmieniono: {s}");
        }
    }

    // TR3: wielkość liter nie może psuć porównania prefiksu — MT kapitalizuje
    // pierwszy wariant na początku zdania; różnica długości 1 znaku mieści się
    // w limicie 2
    #[test]
    fn tr3_dlugie_warianty_i_wielkosc_liter() {
        assert_eq!(
            collapse_variant_slashes("Zainteresowany/zainteresowana tym tematem."),
            "Zainteresowany tym tematem."
        );
        assert_eq!(collapse_variant_slashes("Pewien/pewna."), "Pewien.");
    }

    #[test]
    fn tr4_wiele_ukosnikow_w_jednym_zdaniu() {
        assert_eq!(
            collapse_variant_slashes("Jestem pewien/pewna i zmęczony/zmęczona."),
            "Jestem pewien i zmęczony."
        );
    }

    // TR5: ukośnik bez pełnego słowa po którejś stronie — brak reguły,
    // brak zmiany (i przede wszystkim brak paniki na indeksowaniu)
    #[test]
    fn tr5_ukosnik_na_brzegu_bez_liter() {
        for s in ["/start", "koniec/", "a / b", "//", "/", ""] {
            assert_eq!(collapse_variant_slashes(s), s, "zmieniono: {s}");
        }
    }

    // TR6: udokumentowany, świadomie zaakceptowany fałszywy alarm — patrz doc
    // funkcji. Przy jednym ukośniku na 180 wyjść MT koszt jest pomijalny,
    // ale ma być utrwalony testem, żeby nie był niespodzianką.
    #[test]
    fn tr6_znany_falszywy_alarm_przed_przez() {
        assert_eq!(collapse_variant_slashes("przed/przez"), "przed");
    }

    // TR7: dowód, że cięcie idzie po granicach ZNAKÓW, nie bajtów —
    // "żółty" to 5 znaków i 8 bajtów; cięcie po bajtach paniką by wybuchło
    #[test]
    fn tr7_diakrytyki_utf8_bez_paniki() {
        assert_eq!(collapse_variant_slashes("żółty/żółta"), "żółty");
        assert_eq!(
            collapse_variant_slashes("Ona jest zmęczona/zmęczony, prawda?"),
            "Ona jest zmęczona, prawda?"
        );
    }

    // ---------- rekomendacja 5: polityka kontekstu MT ----------

    /// Translator bez sieci: `LlamaCppTranslator::new` robi llamacpp_check po
    /// HTTP, a nas interesuje wyłącznie polityka historii i składanie promptu.
    fn llama(context_pairs: usize) -> LlamaCppTranslator {
        LlamaCppTranslator {
            cfg: MtCfg {
                context_pairs,
                ..MtCfg::default()
            },
            agent: ureq::AgentBuilder::new().build(),
            history: VecDeque::new(),
        }
    }

    fn pusc_ture(tr: &mut LlamaCppTranslator, n: usize) {
        tr.history
            .push_back(("en".into(), format!("source {n}"), format!("źródło {n}")));
    }

    // TR8: reset zostawia OSTATNIĄ parę (nie pierwszą) — to ona niesie
    // dysambiguator dla następnej tury; w sesji audytu 1/3 tłumaczeń szła
    // z pustym kontekstem i tam wypadło 5 z 10 wpadek MT
    #[test]
    fn tr8_reset_zachowuje_ostatnia_pare() {
        let mut tr = llama(3);
        for n in 1..=3 {
            pusc_ture(&mut tr, n);
        }
        tr.trim_history();
        assert_eq!(tr.history.len(), 1);
        assert_eq!(tr.history[0].1, "source 3");
    }

    // TR9: warunek konieczny z raportu — po resecie prefiks promptu dalej
    // tylko rośnie, więc cache_prompt llama-servera trafia od następnej tury
    #[test]
    fn tr9_prefiks_promptu_zostaje_append_only() {
        let mut tr = llama(3);
        for n in 1..=3 {
            pusc_ture(&mut tr, n);
        }
        tr.trim_history();

        let p1 = tr.build_prompt("en", "t1");
        tr.history
            .push_back(("en".into(), "t1".into(), "r1".into()));
        let p2 = tr.build_prompt("en", "t2");

        assert!(
            p2.starts_with(&p1),
            "prompt przestał być append-only:\n--- p1 ---\n{p1}\n--- p2 ---\n{p2}"
        );
    }

    // TR10: przy context_pairs 0 i 1 nie ma "ostatniej pary" do zachowania —
    // semantyka konfiguracji zostaje dokładnie taka, jak przy dawnym clear()
    #[test]
    fn tr10_degeneraty_context_pairs() {
        for cp in [0, 1] {
            let mut tr = llama(cp);
            pusc_ture(&mut tr, 1);
            tr.trim_history();
            assert!(tr.history.is_empty(), "context_pairs={cp}");
        }
    }
}
