//! LocalAgreement-2 dla spekulacyjnego STT: porównanie dwóch ostatnich
//! przebiegów whispera na rosnącym, OTWARTYM segmencie. Prefiks zgodny na
//! ZNORMALIZOWANYCH SŁOWACH (nie tokenach, nie znakach, bez timestampów)
//! jest cięty WYŁĄCZNIE na interpunkcji frazowej whispera i emitowany do
//! tłumaczenia od razu; przebieg finalny domyka tylko niewyemitowany ogon.
//!
//! Zasada nadrzędna: NIGDY nie cofać emisji; przy niejednoznaczności ZAWSZE
//! duplikat, nigdy ubytek. Zamrożona wczesna pomyłka (dwa przebiegi zgodnie
//! błędne) jest zaakceptowana projektowo — żadnego mechanizmu korekt (M7).
//! Tracker NIE woła HallucinationFilter — filtr działa tylko na finalach (M4).

use std::collections::VecDeque;

/// M5: język zamrażany dla przebiegów częściowych dopiero przy pewnej
/// detekcji i buforze >= 2 s — na krótszym audio detekcja whispera flapuje
pub const LOCK_MIN_MS: u32 = 2_000;
pub const LOCK_MIN_PROB: f32 = 0.8;
/// M3: kotwica licznikowa +/- k słów przy dopasowaniu ogona finalu
const ANCHOR_K: usize = 3;
/// M3: dopasowanie edycyjne ostatnich M scommitowanych słów
const ANCHOR_M: usize = 4;
/// interpunkcja frazowa whispera — jedyne dozwolone miejsca cięcia fragmentu
const PHRASE_PUNCT: &[char] = &['.', ',', '!', '?', ';'];

/// Słowo NAJNOWSZEJ hipotezy: forma znormalizowana (porównania) + zakres
/// bajtowy w surowym tekście (emisja 1:1 tego, co napisał whisper).
struct Word {
    norm: String,
    /// zakres bajtowy w surowym tekście hipotezy, WRAZ z przyklejoną
    /// interpunkcją (token surowy w całości)
    raw: std::ops::Range<usize>,
    /// surowy token kończy się interpunkcją frazową (po odcięciu cudzysłowów
    /// i nawiasów zamykających)
    ends_phrase: bool,
}

/// Ostatni znak tokenu po odrzuceniu końcowych cudzysłowów/nawiasów należy
/// do PHRASE_PUNCT — wielokropek "..." kończy się '.', więc łapie się
/// naturalnie; przecinek WEWNĄTRZ tokenu ("1,000") nie łapie się, bo
/// decyduje znak końcowy.
fn ends_phrase(tok: &str) -> bool {
    tok.trim_end_matches(['"', '”', '\'', '’', ')', ']'])
        .chars()
        .last()
        .map(|c| PHRASE_PUNCT.contains(&c))
        .unwrap_or(false)
}

fn tokenize(raw: &str) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    let mut cursor = 0usize;
    for tok in raw.split_whitespace() {
        // pozycja bajtowa tokenu od przesuwanego kursora: między kursorem
        // a tokenem jest wyłącznie whitespace, więc pierwsze trafienie find
        // to właściwa pozycja, a granice zawsze wypadają na whitespace —
        // slice'y bezpieczne dla UTF-8
        let start = cursor + raw[cursor..].find(tok).unwrap_or(0);
        let end = start + tok.len();
        cursor = end;
        let norm = crate::stt::normalize(tok);
        let ends = ends_phrase(tok);
        if norm.is_empty() {
            // czysta interpunkcja ("—", "..."): nie tworzy słowa — dokleja
            // się do poprzednika (zakres i status frazowy przechodzą na
            // niego), a bez poprzednika przepada. To jedyne miejsce, gdzie
            // liczba słów różni się od liczby tokenów surowych, i jest
            // deterministycznie takie samo dla obu porównywanych przebiegów.
            if let Some(prev) = words.last_mut() {
                prev.raw.end = end;
                prev.ends_phrase = ends;
            }
            continue;
        }
        words.push(Word {
            norm,
            raw: start..end,
            ends_phrase: ends,
        });
    }
    words
}

/// Odległość edycyjna na słowach — DP trywialne, sekwencje mają
/// maks. ANCHOR_M elementów.
fn levenshtein_words(a: &[&str], b: &[&str]) -> usize {
    let mut dp: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut prev = dp[0];
        dp[0] = i;
        for j in 1..=b.len() {
            let tmp = dp[j];
            let cost = usize::from(a[i - 1] != b[j - 1]);
            dp[j] = (dp[j] + 1).min(dp[j - 1] + 1).min(prev + cost);
            prev = tmp;
        }
    }
    dp[b.len()]
}

/// Fragment zaproponowany, ale jeszcze NIE scommitowany — M2: licznik
/// committed wolno przesunąć dopiero po udanym send do kanału mt.
#[derive(Clone, Debug)]
pub struct PendingFragment {
    /// surowy wycinek NAJNOWSZEJ hipotezy, z interpunkcją
    pub text: String,
    /// committed_word_count PO commicie
    pub new_committed: usize,
    /// udział znakowy fragmentu w całej hipotezie — do szacowania orig_secs
    pub char_share: f32,
    /// IDX do logu "#GEN.IDX"
    pub idx: u32,
}

#[derive(Debug)]
pub struct FinalEmit {
    /// ogon do MT ("" = wszystko już wyemitowane fragmentami)
    pub text: String,
    /// udział znakowy ogona w pełnym tekście finalu — proporcja orig_secs
    pub char_share: f32,
    /// dopasowanie kotwicy nie powiodło się — emisja od pozycji licznikowej
    /// minus k (świadomy duplikat); wywołujący loguje re-kotwiczenie
    pub reanchored: bool,
}

pub struct SpeculativeTracker {
    /// generacja, dla której trzymamy stan; None = czysty
    gen: Option<u64>,
    /// wyniki przebiegów z gen < min_gen odrzucane W CAŁOŚCI (M1)
    min_gen: u64,
    /// znormalizowane słowa POPRZEDNIEGO przebiegu (do LCP)
    prev_norms: Option<Vec<String>>,
    /// słowa + surowy tekst NAJNOWSZEGO przebiegu (do emisji i commitu)
    last_words: Vec<Word>,
    last_raw: String,
    /// ile słów hipotezy już wyemitowano do MT
    committed: usize,
    /// ostatnie ANCHOR_M znormalizowanych słów wyemitowanych (kotwica M3)
    committed_tail: VecDeque<String>,
    frag_idx: u32,
    /// M5: język zamrożony dla przebiegów częściowych; reset przy domknięciu
    locked_lang: Option<String>,
    min_fragment_chars: usize,
    // liczniki diagnostyczne (logowane przez pipeline)
    pub reanchors: u64,
    pub lost_tails: u64,
}

impl SpeculativeTracker {
    pub fn new(min_fragment_chars: usize) -> Self {
        Self {
            gen: None,
            min_gen: 0,
            prev_norms: None,
            last_words: Vec::new(),
            last_raw: String::new(),
            committed: 0,
            committed_tail: VecDeque::with_capacity(ANCHOR_M),
            frag_idx: 0,
            locked_lang: None,
            min_fragment_chars,
            reanchors: 0,
            lost_tails: 0,
        }
    }

    /// Tani test przed transkrypcją: przebieg starszej generacji w całości
    /// do kosza (M1) — zanim zapłacimy za whispera.
    pub fn is_stale(&self, gen: u64) -> bool {
        gen < self.min_gen
    }

    pub fn locked_lang(&self) -> Option<&str> {
        self.locked_lang.as_deref()
    }

    pub fn lock_lang(&mut self, code: &str) {
        self.locked_lang = Some(code.to_string());
    }

    /// Wynik przebiegu częściowego. Zmiana generacji => twardy reset stanu
    /// (M1: żadnego re-kotwiczenia przez granicę generacji). Zwraca
    /// propozycję fragmentu; NIE przesuwa committed (M2).
    pub fn on_partial(&mut self, gen: u64, raw: &str) -> Option<PendingFragment> {
        if gen < self.min_gen {
            return None; // stale — licznik loguje wywołujący
        }
        if self.gen != Some(gen) {
            if self.committed > 0 {
                self.lost_tails += 1;
                log::warn!(
                    "spekulacja: gen {} porzucona z {} scommitowanymi słowami bez finalu — \
                     ogon przepadł (strat łącznie: {})",
                    self.gen.unwrap_or(0),
                    self.committed,
                    self.lost_tails
                );
            }
            self.reset_to(Some(gen));
        }

        let cur = tokenize(raw);
        let cur_norms: Vec<String> = cur.iter().map(|w| w.norm.clone()).collect();
        let prev = self.prev_norms.replace(cur_norms);
        self.last_words = cur;
        self.last_raw = raw.to_string();
        // pierwszy przebieg tej generacji — nie ma z czym się zgodzić
        let prev = prev?;

        // LCP na znormalizowanych słowach (LocalAgreement-2): stabilne jest
        // to, co dwa kolejne przebiegi napisały tak samo
        let lcp = prev
            .iter()
            .zip(self.last_words.iter())
            .take_while(|(p, c)| p.as_str() == c.norm)
            .count();
        if lcp <= self.committed {
            return None; // nic nowego stabilnego
        }

        // cięcie WYŁĄCZNIE na interpunkcji frazowej: największe j w
        // (committed..=lcp], gdzie słowo j-1 kończy frazę
        let j = (self.committed + 1..=lcp)
            .rev()
            .find(|&j| self.last_words[j - 1].ends_phrase)?;

        let start_b = self.last_words[self.committed].raw.start;
        let end_b = self.last_words[j - 1].raw.end;
        let text = self.last_raw[start_b..end_b].trim().to_string();
        // bramki fragmentu (M4): wyłącznie minimalna długość (>=3 słowa lub
        // >=min_fragment_chars znaków) — za krótki poczeka i urośnie
        let n_words = j - self.committed;
        if n_words < 3 && text.chars().count() < self.min_fragment_chars {
            return None;
        }
        let char_share =
            text.chars().count() as f32 / self.last_raw.chars().count().max(1) as f32;
        Some(PendingFragment {
            text,
            new_committed: j,
            char_share,
            idx: self.frag_idx,
        })
    }

    /// Wywołać WYŁĄCZNIE po tym, jak mt_tx.send(fragment) zwrócił Ok (M2) —
    /// commit to obietnica dostarczenia.
    pub fn commit(&mut self, p: &PendingFragment) {
        for w in &self.last_words[self.committed..p.new_committed] {
            if self.committed_tail.len() == ANCHOR_M {
                self.committed_tail.pop_front();
            }
            self.committed_tail.push_back(w.norm.clone());
        }
        self.committed = p.new_committed;
        self.frag_idx += 1;
    }

    /// Wynik przebiegu finalnego: wyznacza niewyemitowany ogon dopasowaniem
    /// POZYCYJNYM (M3), resetuje stan segmentu i przesuwa min_gen na gen+1.
    /// `coalesced`: ile segmentów skleiła koalescencja w stt_thread.
    /// `raw`: tekst finalu PO deduplikacji szwu.
    pub fn on_final(&mut self, gen: u64, coalesced: usize, raw: &str) -> FinalEmit {
        // gen niezgodna = commity dotyczyły INNEGO audio, pełna emisja
        // niczego nie duplikuje; brak commitów = nie ma czego odejmować
        if self.gen != Some(gen) || self.committed == 0 {
            self.min_gen = self.min_gen.max(gen + 1);
            self.reset_to(None);
            return FinalEmit {
                text: raw.trim().to_string(),
                char_share: 1.0,
                reanchored: false,
            };
        }

        let f = tokenize(raw);
        let c = self.committed;
        let m = ANCHOR_M.min(c);
        let anchor: Vec<&str> = self.committed_tail.iter().map(|s| s.as_str()).collect();
        debug_assert_eq!(anchor.len(), m); // tail trzyma min(committed, ANCHOR_M) norm

        // M3: zakres startów okna — kotwica licznikowa ±k (okno absorbuje
        // też przesunięcie o 1 słowo po deduplikacji szwu); przy koalescencji
        // starsze segmenty doklejone PRZED audio tej generacji unieważniają
        // licznik pozycyjny, więc okno rozszerzamy na całą listę
        // (rozszerzenie reguły "brak dopasowania → duplikat, nie ubytek")
        let (w_lo, w_hi) = if coalesced == 1 {
            let base = c - m;
            (base.saturating_sub(ANCHOR_K), (base + ANCHOR_K + 1).min(f.len()))
        } else {
            (0, f.len())
        };
        let tol = if m >= ANCHOR_M { 2 } else { 1 };

        let mut matched = None;
        for w in w_lo..w_hi {
            let win: Vec<&str> = f[w..(w + m).min(f.len())]
                .iter()
                .map(|x| x.norm.as_str())
                .collect();
            if levenshtein_words(&anchor, &win) <= tol {
                // przy WIELU kandydatach ZAWSZE najwcześniejszy — gwarancja
                // duplikacji, nie ubytku
                matched = Some(w + m);
                break;
            }
        }
        let (start, reanchored) = match matched {
            Some(s) => (s, false),
            None => {
                // brak dopasowania: emisja od pozycji licznikowej minus k —
                // świadomy duplikat zamiast ryzyka ubytku
                self.reanchors += 1;
                (c.saturating_sub(ANCHOR_K), true)
            }
        };

        let text = if start >= f.len() {
            // final w całości pokryty fragmentami (także final KRÓTSZY niż
            // committed — rewizja w dół, bez korekt: M7)
            String::new()
        } else {
            raw[f[start].raw.start..].trim().to_string()
        };
        let char_share = if text.is_empty() {
            0.0
        } else {
            text.chars().count() as f32 / raw.chars().count().max(1) as f32
        };

        self.min_gen = self.min_gen.max(gen + 1);
        self.reset_to(None);
        FinalEmit {
            text,
            char_share,
            reanchored,
        }
    }

    /// Final istniał, ale nie będzie emitowany (odrzucony przez filtr /
    /// budżet wieku / błąd whispera) — sam reset i przesunięcie min_gen;
    /// scommitowany ogon przepada (strata zaakceptowana świadomie).
    pub fn on_final_rejected(&mut self, gen: u64) {
        if self.gen == Some(gen) && self.committed > 0 {
            self.lost_tails += 1;
            log::warn!(
                "spekulacja: gen {gen} — final odrzucony po {} scommitowanych słowach, \
                 ogon przepadł (strat łącznie: {})",
                self.committed,
                self.lost_tails
            );
        }
        self.min_gen = self.min_gen.max(gen + 1);
        self.reset_to(None);
    }

    /// Czyści CAŁY stan segmentu, łącznie z locked_lang (M5: lock per segment).
    fn reset_to(&mut self, gen: Option<u64>) {
        self.gen = gen;
        self.prev_norms = None;
        self.last_words.clear();
        self.last_raw.clear();
        self.committed = 0;
        self.committed_tail.clear();
        self.frag_idx = 0;
        self.locked_lang = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> SpeculativeTracker {
        SpeculativeTracker::new(12)
    }

    /// tracker z 4 scommitowanymi słowami [alpha, bravo, charlie, delta]
    fn committed_abcd(gen: u64) -> SpeculativeTracker {
        let mut tr = tracker();
        assert!(tr.on_partial(gen, "alpha bravo charlie delta, echo").is_none());
        let frag = tr
            .on_partial(gen, "alpha bravo charlie delta, echo fox")
            .expect("stabilny prefiks z przecinkiem");
        assert_eq!(frag.text, "alpha bravo charlie delta,");
        assert_eq!(frag.new_committed, 4);
        tr.commit(&frag);
        tr
    }

    // T1: dwa zgodne przebiegi → fragment cięty na przecinku, tekst surowy
    // z interpunkcją, char_share liczony znakami
    #[test]
    fn t1_zgodne_przebiegi() {
        let mut tr = tracker();
        assert!(tr.on_partial(1, "Hello there, how are").is_none());
        let frag = tr
            .on_partial(1, "Hello there, how are you")
            .expect("fragment po zgodzie przebiegów");
        assert_eq!(frag.text, "Hello there,");
        assert_eq!(frag.new_committed, 2);
        assert!((frag.char_share - 0.5).abs() < 1e-6); // 12 / 24 znaków
    }

    // T2: rozjazd końcówki — LCP staje przed rozjazdem, emisja tylko do
    // ostatniej interpunkcji w stabilnej części
    #[test]
    fn t2_rozjazd_koncowki() {
        let mut tr = tracker();
        assert!(tr
            .on_partial(1, "All right then, we are going to the store now")
            .is_none());
        let frag = tr
            .on_partial(1, "All right then, we are going to the shop today")
            .expect("stabilny prefiks przed rozjazdem");
        // zgoda sięga do "the" (8 słów), ale interpunkcja tylko po "then,"
        assert_eq!(frag.text, "All right then,");
        assert_eq!(frag.new_committed, 3);
    }

    // T3: normalizacja zmienia liczbę słów — czysta interpunkcja wchłonięta
    // przez poprzednika, "1,000" jednym słowem bez ends_phrase
    #[test]
    fn t3_tokenizacja_interpunkcji() {
        let raw = "So ... we wait";
        let w = tokenize(raw);
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].norm, "so");
        assert_eq!(&raw[w[0].raw.clone()], "So ...");
        assert!(w[0].ends_phrase); // "..." kończy się '.'

        let raw = "he paid 1,000 dollars";
        let w = tokenize(raw);
        assert_eq!(w.len(), 4);
        assert_eq!(w[2].norm, "1000");
        assert!(!w[2].ends_phrase); // przecinek WEWNĄTRZ tokenu nie kończy frazy

        // myślnik po przecinku przejmuje status frazowy poprzednika (ostatni
        // znak scalonego tokenu to '—', nie interpunkcja frazowa)
        let raw = "Well, — sure";
        let w = tokenize(raw);
        assert_eq!(w.len(), 2);
        assert_eq!(&raw[w[0].raw.clone()], "Well, —");
        assert!(!w[0].ends_phrase);

        // interpunkcja bez poprzednika przepada
        let w = tokenize("— hello");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].norm, "hello");
    }

    // T4: zmiana generacji (forced cut) → twardy reset: committed=0, brak
    // LCP z poprzednikiem starej gen, lost_tails rośnie
    #[test]
    fn t4_forced_cut_resetuje_generacje() {
        let mut tr = committed_abcd(5);
        assert_eq!(tr.lost_tails, 0);
        // pierwszy przebieg gen 6 nic nie emituje nawet przy identycznym tekście
        assert!(tr.on_partial(6, "alpha bravo charlie delta, echo fox").is_none());
        assert_eq!(tr.lost_tails, 1); // commit gen 5 przepadł bez finalu
        // drugi przebieg gen 6 emituje od ZERA (committed zresetowany)
        let frag = tr
            .on_partial(6, "alpha bravo charlie delta, echo fox")
            .expect("zgoda dwóch przebiegów gen 6");
        assert_eq!(frag.text, "alpha bravo charlie delta,");
        assert_eq!(frag.new_committed, 4);
    }

    // T5: final z rewizją za scommitowanym prefiksem — kotwica trafia
    // (dist=0 na pierwszym kandydacie okna), ogon zaczyna się dokładnie na
    // rewizji i jest wyemitowany raz; lock języka znika po finale
    #[test]
    fn t5_ogon_z_rewizja() {
        let mut tr = committed_abcd(7);
        tr.lock_lang("en");
        let emit = tr.on_final(7, 1, "alpha bravo charlie delta, revised tail here.");
        assert!(!emit.reanchored);
        assert_eq!(emit.text, "revised tail here.");
        assert_eq!(tr.reanchors, 0);
        // M5: lock per segment — final resetuje
        assert!(tr.locked_lang().is_none());
        assert!(tr.is_stale(7));
    }

    // T6: final całkowicie przepisany → brak dopasowania kotwicy, emisja od
    // committed − k (duplikaty słów, zero ubytku), licznik re-kotwiczeń rośnie
    #[test]
    fn t6_brak_dopasowania_duplikat() {
        let mut tr = committed_abcd(1);
        let emit = tr.on_final(1, 1, "zulu yankee xray whiskey victor uniform tango sierra");
        assert!(emit.reanchored);
        assert_eq!(tr.reanchors, 1);
        // start = committed(4) − k(3) = 1 → emisja od "yankee"
        assert_eq!(emit.text, "yankee xray whiskey victor uniform tango sierra");
    }

    // T7: za krótki fragment czeka; po dorośnięciu emisja CAŁOŚCI od zera
    #[test]
    fn t7_za_krotki_fragment_rosnie() {
        let mut tr = tracker();
        assert!(tr.on_partial(1, "Yes, ok,").is_none()); // pierwszy przebieg
        // 2 słowa, 8 znaków < 12 → bramka długości, committed bez zmian
        assert!(tr.on_partial(1, "Yes, ok,").is_none());
        assert!(tr.on_partial(1, "Yes, ok, we are done. And").is_none()); // LCP=2, wciąż za krótko
        let frag = tr
            .on_partial(1, "Yes, ok, we are done. And so")
            .expect("fragment po dorośnięciu");
        assert_eq!(frag.text, "Yes, ok, we are done.");
        assert_eq!(frag.new_committed, 5);
    }

    // T8: pełna zgoda przebiegów, ale zero interpunkcji frazowej → brak emisji
    #[test]
    fn t8_brak_interpunkcji() {
        let mut tr = tracker();
        assert!(tr
            .on_partial(1, "this is a long stable prefix without punctuation")
            .is_none());
        assert!(tr
            .on_partial(1, "this is a long stable prefix without punctuation marks")
            .is_none());
    }

    // T9: dwufazowość M2 — bez commitu kolejny przebieg proponuje ten sam
    // fragment od tej samej pozycji (z tym samym idx)
    #[test]
    fn t9_dwufazowosc_bez_commitu() {
        let mut tr = tracker();
        let raw = "All right then, we go";
        assert!(tr.on_partial(2, raw).is_none());
        let a = tr.on_partial(2, raw).expect("propozycja 1");
        // BEZ commitu — propozycja musi się powtórzyć identycznie
        let b = tr.on_partial(2, raw).expect("propozycja 2");
        assert_eq!(a.text, b.text);
        assert_eq!(a.new_committed, b.new_committed);
        assert_eq!(a.idx, b.idx);
    }

    // T10: final skoalescowany — okno na całej liście, kotwica występuje
    // dwa razy, wygrywa NAJWCZEŚNIEJSZY start (duplikat, nie ubytek: wybór
    // późniejszego zgubiłby "one two...")
    #[test]
    fn t10_final_skoalescowany_najwczesniejszy() {
        let mut tr = committed_abcd(3);
        let emit = tr.on_final(
            3,
            2,
            "alpha bravo charlie delta one two alpha bravo charlie delta end",
        );
        assert!(!emit.reanchored);
        assert_eq!(emit.text, "one two alpha bravo charlie delta end");
    }

    // T11: stale gen — po finale starsza generacja jest martwa i nie dotyka
    // stanu; następna generacja działa normalnie
    #[test]
    fn t11_stale_gen() {
        let mut tr = tracker();
        assert!(tr.on_partial(3, "one two, three").is_none());
        let emit = tr.on_final(3, 1, "one two, three");
        assert_eq!(emit.text, "one two, three"); // committed==0 → pełna emisja
        assert!(tr.is_stale(3));
        assert!(!tr.is_stale(4));
        // spóźniona migawka gen 3: None i zero skutków ubocznych
        assert!(tr.on_partial(3, "whatever text here,").is_none());
        // gen 4 startuje czysto: pierwszy przebieg None, drugi emituje
        assert!(tr.on_partial(4, "alpha bravo charlie delta, echo").is_none());
        let frag = tr
            .on_partial(4, "alpha bravo charlie delta, echo fox")
            .expect("gen 4 działa normalnie");
        assert_eq!(frag.new_committed, 4);
    }

    // T12: final krótszy niż committed (rewizja w dół) → pusty ogon, bez paniki
    #[test]
    fn t12_final_krotszy_niz_committed() {
        let mut tr = committed_abcd(9);
        let emit = tr.on_final(9, 1, "alpha bravo");
        assert_eq!(emit.text, "");
        assert_eq!(emit.char_share, 0.0);
    }

    // T13: UTF-8 — zakresy bajtowe tną poprawnie na polskich znakach
    #[test]
    fn t13_utf8_diakrytyki() {
        let mut tr = tracker();
        assert!(tr.on_partial(1, "Zażółć gęślą jaźń, w ogóle").is_none());
        let frag = tr
            .on_partial(1, "Zażółć gęślą jaźń, w ogóle też")
            .expect("fragment z diakrytykami");
        assert_eq!(frag.text, "Zażółć gęślą jaźń,");
        assert_eq!(frag.new_committed, 3);
    }
}
