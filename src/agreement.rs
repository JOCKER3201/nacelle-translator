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
/// co ile resynchronizacji licznika podnosić diagnostykę z DEBUG do INFO —
/// w całym 7-minutowym logu audytu było 0 linii DEBUG, więc rozjazdy kotwicy
/// trzeba było wnioskować pośrednio z niezmienników kodu zamiast czytać z logu
const RESYNC_LOG_EVERY: u64 = 10;
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
    /// generacje finalu miały scommitowane fragmenty — commit to obietnica
    /// dostarczenia, więc wywołujący NIE może porzucić ogona bramką
    /// skip_target_lang (rozjazd lock vs detekcja finalu = duplikat, nie ubytek)
    pub had_commits: bool,
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
    /// resync przesunął licznik W PRZÓD (hipoteza urosła / przesunęła się
    /// w prawo) — nieszkodliwe
    pub resyncs_up: u64,
    /// resync COFNĄŁ licznik — każdy taki przypadek to GWARANTOWANY duplikat
    /// u lektora; to jest KPI rekomendacji 3. Część cofnięć jest legalna:
    /// rewizja scalająca "it is"->"it's" skraca prefiks (T14), więc licznik
    /// nie ma spaść do zera, tylko wyraźnie w dół
    pub resyncs_down: u64,
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
            resyncs_up: 0,
            resyncs_down: 0,
        }
    }

    /// Tani test przed transkrypcją: przebieg starszej generacji w całości
    /// do kosza (M1) — zanim zapłacimy za whispera.
    pub fn is_stale(&self, gen: u64) -> bool {
        gen < self.min_gen
    }

    /// Czy któraś generacja z zakresu [gen_oldest, gen] ma scommitowane
    /// fragmenty — tani odczyt dla bramek w stt_thread (wiek/filtr): final
    /// domykający commity ma obowiązek dostarczenia ogona, nie wolno go
    /// porzucić hurtem.
    pub fn has_commits(&self, gen_oldest: u64, gen: u64) -> bool {
        self.committed > 0 && self.gen.is_some_and(|g| gen_oldest <= g && g <= gen)
    }

    /// Kotwica trafia DOKŁADNIE na pozycji licznikowej (dist=0) — zero dryfu,
    /// emisja pozycyjna bez przeszukiwania. Długość kotwicy bierzemy z tailu,
    /// nie z licznika: resync w dół może zepchnąć licznik poniżej długości
    /// tailu, a kotwica to zawsze OSTATNIE wyemitowane słowa w komplecie.
    fn anchor_exact_at(&self, words: &[Word]) -> bool {
        let m = self.committed_tail.len();
        if m == 0 || self.committed < m {
            return false;
        }
        let base = self.committed - m;
        base + m <= words.len()
            && words[base..base + m]
                .iter()
                .zip(self.committed_tail.iter())
                .all(|(w, a)| w.norm == *a)
    }

    /// Szuka kotwicy (committed_tail) w `words[w_lo..w_hi]` (starty okien);
    /// zwraca pozycję ZA dopasowanym oknem = pierwsze niewyemitowane słowo.
    /// Przy WIELU kandydatach wygrywa NAJWCZEŚNIEJSZY start (duplikat, nie
    /// ubytek). Oprócz okna długości m próbujemy też m−1: rewizja scalająca
    /// słowa WEWNĄTRZ kotwicy ("it is"→"it's") skraca ją w nowej hipotezie
    /// o słowo, a sztywne okno m dolicza wtedy zbędną edycję za doklejone
    /// obce słowo i dopasowanie przepada mimo realnej zgody.
    fn find_anchor(&self, words: &[Word], w_lo: usize, w_hi: usize) -> Option<usize> {
        let m = self.committed_tail.len();
        let anchor: Vec<&str> = self.committed_tail.iter().map(|s| s.as_str()).collect();
        let tol = if m >= ANCHOR_M { 2 } else { 1 };
        for w in w_lo..w_hi.min(words.len()) {
            for len in [m, m.saturating_sub(1)] {
                if len == 0 {
                    continue;
                }
                let win: Vec<&str> = words[w..(w + len).min(words.len())]
                    .iter()
                    .map(|x| x.norm.as_str())
                    .collect();
                if levenshtein_words(&anchor, &win) <= tol {
                    return Some(w + len);
                }
            }
        }
        None
    }

    /// Wariant kotwiczenia dla OKNA LICZNIKOWEGO ±ANCHOR_K: najpierw przebieg
    /// z tol=0 i len=m (dopasowanie IDEALNE po tym samym zakresie okien),
    /// dopiero potem obecna pętla rozmyta.
    ///
    /// DLACZEGO: dla kotwicy 4-słowowej okno przesunięte o JEDNO słowo ma
    /// odległość Levenshteina dokładnie 2 (jedno usunięcie + jedno wstawienie),
    /// a tol dla m >= ANCHOR_M wynosi 2 — więc fałszywe okno ZAWSZE mieści się
    /// w tolerancji, a że jest skanowane wcześniej, ZAWSZE wygrywa z prawdziwym.
    /// Licznik committed cofa się i lektor powtarza wypowiedziane już słowa
    /// (audyt: 9 par fragmentów + ~11 ogonów finali = ~26 słów = 1,9 % tekstu
    /// do MT; #43 "ventilation" dwa razy, #69 "below in the comments" TRZY razy
    /// w ciągu 4 sekund). Defekt jest lepki: po cofnięciu anchor_exact_at
    /// przestaje trafiać i pętla rozmyta bez końca potwierdza złą pozycję —
    /// przebieg tol=0 wyprowadza tracker z tego stanu.
    ///
    /// ZAKRES CELOWO OGRANICZONY do okna ±ANCHOR_K. Na wywołaniach z PEŁNĄ
    /// listą (on_final: 0..f.len()) preferencja idealnego dopasowania jest
    /// ZAKAZANA — przy powtarzalnych frazach ("all the way ... all the way up
    /// again") późniejsze dopasowanie idealne pobiłoby wcześniejsze prawdziwe
    /// z d=1 i emisja przeskoczyłaby całe zdanie (zweryfikowany ubytek 8 słów,
    /// pilnuje tego T20).
    ///
    /// PRZEBIEG IDEALNY WYŁĄCZNIE DLA PEŁNEJ KOTWICY (m == ANCHOR_M). Kotwica
    /// jest krótsza po każdym PIERWSZYM fragmencie segmentu liczącym mniej niż
    /// 4 słowa, a bramka fragmentu wprost takie dopuszcza (2 słowa o >=
    /// min_fragment_chars znakach przechodzą) — w sesji audytu 12 z 63
    /// segmentów otwierało się fragmentem 2- lub 3-słowowym. Przy m < ANCHOR_M
    /// przebieg idealny NIC NIE KUPUJE, bo `find_anchor` ma tam tol = 1, a
    /// naprawiane fałszywe okno przesunięte o jedno słowo ma d = 2 i przez tę
    /// tolerancję i tak nie przechodzi. Wnosi za to ubytek: warunkiem przeskoku
    /// przestaje być powtórzony 4-gram, a staje się powtórzony BI-GRAM
    /// ("absolutely fantastic, absolutely fantastic"), co w mowie potocznej jest
    /// codziennością. Zweryfikowany kontrprzykład dla m = 2: pętla rozmyta
    /// zwraca 2 (duplikat, bezpiecznie), przebieg idealny bez tej bramki
    /// zwracał 4 i dwa słowa NIGDY nie trafiały do lektora — pilnuje tego T22.
    ///
    /// Bilans najgorszego przypadku PO tej bramce: ubytek względem pętli
    /// rozmytej jest ograniczony zakresem okna do 2*ANCHOR_K + 1 = 7 słów
    /// i wymaga, żeby IDENTYCZNY 4-gram powtórzył się 1-3 słowa ZA kotwicą
    /// (wcześniejsze powtórzenie jest bezpieczne — skan idzie od w_lo i zwraca
    /// najwcześniejsze trafienie, czyli duplikat) przy jednoczesnej rewizji
    /// prawdziwego wystąpienia. W analizowanej sesji taki układ nie wystąpił
    /// ani razu, a od teraz jest WIDOCZNY: resync o więcej niż jedno słowo
    /// podnosi WARN w on_partial (oba udokumentowane defekty to skok o
    /// dokładnie 1, bo whisper dokłada/zabiera JEDNO słowo przed granicą).
    /// Rozważone i odrzucone: zawężenie przebiegu idealnego do +/-1. Zdjęłoby
    /// resztkowe ryzyko, ale kosztem dopasowań przy dryfie licznika o 2-3 słowa,
    /// które w tym materiale realnie występują (front hipotezy #22 przesunął się
    /// o 3 słowa) — a tam brak trafienia oznacza powrót duplikatu.
    ///
    /// Skala zysku, skorygowana pomiarem po fakcie: ~26 słów to ~5-6 s czasu
    /// lektora (nie ~8 s — jedna z 10 par, #6 "quieter", NIE jest artefaktem,
    /// mówca faktycznie powtórzył słowo trzy razy). Duplikaty nie rozkładają się
    /// równomiernie: sam segment #69 wnosi ~2,4 s w ostatnich 20 s materiału,
    /// więc GŁÓWNYM efektem jest skrócenie OGONA, a nie średniego obciążenia.
    fn find_anchor_windowed(&self, words: &[Word], w_lo: usize, w_hi: usize) -> Option<usize> {
        let m = self.committed_tail.len();
        if m >= ANCHOR_M {
            for w in w_lo..w_hi.min(words.len()) {
                // okno MUSI mieścić się w całości: obcięte na końcu listy nie
                // jest "dopasowaniem idealnym" (final krótszy niż committed —
                // T12 ma spaść na pętlę rozmytą, nie na fałszywe trafienie)
                if w + m > words.len() {
                    break;
                }
                if words[w..w + m]
                    .iter()
                    .zip(self.committed_tail.iter())
                    .all(|(x, a)| x.norm == *a)
                {
                    // reguła "najwcześniejszy start wygrywa przy remisie"
                    // zachowana: skan idzie od w_lo w górę i zwraca PIERWSZE
                    // trafienie idealne
                    return Some(w + m);
                }
            }
        }
        self.find_anchor(words, w_lo, w_hi)
    }

    /// M3 dla przebiegów częściowych: committed jest licznikiem w jednostkach
    /// pozycji hipotezy Z CHWILI COMMITU — weryfikacja kotwicy na BIEŻĄCEJ
    /// hipotezie zwraca licznik w jej jednostkach (dokładne trafienie
    /// pozycyjne wygrywa; inaczej najwcześniejsze okno w ±k). None = kotwica
    /// nieodnaleziona: emisję w tym przebiegu trzeba pominąć.
    fn resync_committed(&self) -> Option<usize> {
        if self.anchor_exact_at(&self.last_words) {
            return Some(self.committed);
        }
        let base = self.committed.saturating_sub(self.committed_tail.len());
        let w_lo = base.saturating_sub(ANCHOR_K);
        let w_hi = base + ANCHOR_K + 1;
        self.find_anchor_windowed(&self.last_words, w_lo, w_hi)
            .map(|e| e.min(self.last_words.len()))
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

        // Rewizja whispera zmniejszająca liczbę słów przed granicą commitu
        // ("it is"→"it's", znikające słowo-halucynacja) przesuwa indeksy
        // w lewo i emisja pozycyjna od last_words[committed] przeskoczyłaby
        // prawdziwe, nigdy niewyemitowane słowo. Przed emisją weryfikujemy
        // kotwicę (jak w on_final) i resynchronizujemy licznik do bieżącej
        // hipotezy; brak dopasowania = pominięcie emisji w TYM przebiegu
        // (bezpieczne — nic nie ginie, ogon załatwi kotwica finalu). Resync
        // ogranicza przy okazji skumulowany dryf licznika między commitami.
        if self.committed > 0 {
            match self.resync_committed() {
                Some(c) if c != self.committed => {
                    log::debug!(
                        "spekulacja: gen {gen} resync licznika {} → {c} po rewizji hipotezy",
                        self.committed
                    );
                    if c > self.committed {
                        self.resyncs_up += 1;
                        // Jedyna obserwowalna sygnatura resztkowego ryzyka
                        // ubytku z find_anchor_windowed: uzasadniony resync
                        // w przód to ZAWSZE skok o 1 (whisper dokłada jedno
                        // słowo przed granicą commitu — #43, #69). Skok o 2-3
                        // znaczy, że przebieg idealny trafił w POWTÓRZONĄ frazę
                        // za kotwicą, a wtedy przeskoczone słowa nigdy nie
                        // pójdą do lektora. WARN, nie DEBUG: w całym logu
                        // audytu nie było ani jednej linii DEBUG.
                        if c > self.committed + 1 {
                            log::warn!(
                                "spekulacja: gen {gen} resync W PRZÓD o {} słów ({} → {c}) — \
                                 sprawdź powtórzoną frazę przed granicą commitu \
                                 (ryzyko pominięcia słów)",
                                c - self.committed,
                                self.committed
                            );
                        }
                    } else {
                        self.resyncs_down += 1;
                    }
                    // DEBUG jest w praktyce niewidoczny (0 linii DEBUG w całym
                    // logu audytu), a resync w dół to jedyny obserwowalny ślad
                    // duplikatu u lektora — okresowe INFO jest KPI rekomendacji 3
                    let total = self.resyncs_up + self.resyncs_down;
                    if total % RESYNC_LOG_EVERY == 0 {
                        log::info!(
                            "spekulacja: resyncy licznika kotwicy — {} w górę, {} w dół \
                             (każdy w dół = powtórka u lektora)",
                            self.resyncs_up,
                            self.resyncs_down
                        );
                    }
                    self.committed = c;
                }
                Some(_) => {}
                None => return None,
            }
        }

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

    /// Wywołać WYŁĄCZNIE po udanym umieszczeniu fragmentu w kanale mt
    /// (try_send zwrócił Ok) — commit to obietnica dostarczenia (M2).
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
    /// `gen_oldest`: NAJSTARSZA generacja sklejki (koalescencja w stt_thread
    /// dokleja starsze segmenty z PRZODU i raportuje najnowszą gen) — commity
    /// trackera dla którejkolwiek gen z [gen_oldest, gen] dotyczą audio
    /// zawartego w tym finale, więc deduplikujemy je kotwicą zamiast
    /// re-emitować hurtem.
    /// `coalesced`: ile segmentów skleiła koalescencja w stt_thread.
    /// `raw`: tekst finalu PO deduplikacji szwu.
    pub fn on_final(&mut self, gen: u64, gen_oldest: u64, coalesced: usize, raw: &str) -> FinalEmit {
        let same_audio = self.gen.is_some_and(|g| gen_oldest <= g && g <= gen);
        // gen spoza sklejki = commity dotyczyły INNEGO audio (np. segment
        // zgubiony na pełnym seg_tx), pełna emisja niczego nie duplikuje;
        // brak commitów = nie ma czego odejmować
        if !same_audio || self.committed == 0 {
            if !same_audio && self.committed > 0 {
                self.lost_tails += 1;
                log::warn!(
                    "spekulacja: gen {} porzucona z {} scommitowanymi słowami — final \
                     innych generacji [{gen_oldest}..{gen}] (strat łącznie: {})",
                    self.gen.unwrap_or(0),
                    self.committed,
                    self.lost_tails
                );
            }
            self.min_gen = self.min_gen.max(gen + 1);
            self.reset_to(None);
            return FinalEmit {
                text: raw.trim().to_string(),
                char_share: 1.0,
                reanchored: false,
                had_commits: false,
            };
        }

        let f = tokenize(raw);
        let c = self.committed;
        // licznik pozycyjny jest w jednostkach hipotez częściowych TEJ
        // generacji — ważny tylko bez sklejki (starsze segmenty doklejone
        // z przodu przesuwają wszystko w prawo)
        let counter_valid = coalesced == 1 && self.gen == Some(gen);
        let mut reanchored = false;
        let start = if counter_valid && self.anchor_exact_at(&f) {
            c // dokładne trafienie pozycyjne — zero dryfu
        } else {
            let base = c.saturating_sub(self.committed_tail.len());
            let matched = if counter_valid {
                // M3: okno licznikowe ±k (absorbuje też przesunięcie o 1
                // słowo po deduplikacji szwu)
                self.find_anchor_windowed(&f, base.saturating_sub(ANCHOR_K), base + ANCHOR_K + 1)
                    .or_else(|| {
                        // dryf licznika ponad ±k (skumulowane rewizje
                        // zmniejszające liczbę słów przed granicą): zanim
                        // spadniemy na fallback licznikowy, przeszukujemy
                        // kotwicą CAŁĄ listę — najwcześniejsze dopasowanie
                        // to duplikat, nie ubytek
                        let full = self.find_anchor(&f, 0, f.len());
                        if full.is_some() {
                            log::info!(
                                "spekulacja: gen {gen} kotwica poza oknem ±k — \
                                 dopasowana pełną listą (dryf licznika)"
                            );
                        }
                        full
                    })
            } else {
                // sklejka / commity starszej gen: licznik nieważny — od razu
                // cała lista, najwcześniejsze dopasowanie
                self.find_anchor(&f, 0, f.len())
            };
            match matched {
                Some(e) => e,
                None => {
                    // brak dopasowania: emisja od pozycji licznikowej minus
                    // k — świadomy duplikat zamiast ryzyka ubytku
                    self.reanchors += 1;
                    reanchored = true;
                    c.saturating_sub(ANCHOR_K)
                }
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
            had_commits: true,
        }
    }

    /// Final istniał, ale nie będzie emitowany (odrzucony przez budżet wieku /
    /// błąd whispera / pusty tekst) — sam reset i przesunięcie min_gen;
    /// scommitowany ogon przepada (strata zaakceptowana świadomie).
    /// `gen_oldest`..`gen`: zakres generacji sklejki, jak w on_final.
    pub fn on_final_rejected(&mut self, gen_oldest: u64, gen: u64) {
        if self.committed > 0 && self.gen.is_some_and(|g| gen_oldest <= g && g <= gen) {
            self.lost_tails += 1;
            log::warn!(
                "spekulacja: gen [{gen_oldest}..{gen}] — final odrzucony po {} scommitowanych \
                 słowach, ogon przepadł (strat łącznie: {})",
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
        let emit = tr.on_final(7, 7, 1, "alpha bravo charlie delta, revised tail here.");
        assert!(!emit.reanchored);
        assert!(emit.had_commits);
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
        let emit = tr.on_final(1, 1, 1, "zulu yankee xray whiskey victor uniform tango sierra");
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
        let emit = tr.on_final(3, 3, 1, "one two, three");
        assert_eq!(emit.text, "one two, three"); // committed==0 → pełna emisja
        assert!(!emit.had_commits);
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
        let emit = tr.on_final(9, 9, 1, "alpha bravo");
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

    // T14: rewizja SCALAJĄCA słowa przed granicą commitu ("it is"→"it's")
    // przesuwa indeksy w lewo — bez resyncu emisja pozycyjna przeskoczyłaby
    // "and" (bezwrotny ubytek); z resynciem "and" wychodzi we fragmencie
    #[test]
    fn t14_rewizja_scalajaca_resynchronizuje() {
        let mut tr = tracker();
        assert!(tr.on_partial(1, "I think it is red, and then").is_none());
        let frag = tr
            .on_partial(1, "I think it is red, and then we")
            .expect("commit prefiksu w starej formie");
        assert_eq!(frag.text, "I think it is red,");
        assert_eq!(frag.new_committed, 5);
        tr.commit(&frag);
        // rewizja scalona: jedno słowo mniej przed granicą; resync 5 → 4,
        // LCP ze starą hipotezą za mały na emisję
        assert!(tr.on_partial(1, "I think it's red, and then we go").is_none());
        // kanarek uzasadnionego resyncu W DÓŁ: kotwica ["think","it","is","red"]
        // nie ma dopasowania DOKŁADNEGO w "I think it's red..." (normalize("it's")
        // == "its"), więc przebieg tol=0 nic nie znajduje i sterowanie spada na
        // pętlę rozmytą — legalne cofnięcia licznika mają dalej działać
        assert_eq!(tr.resyncs_down, 1);
        // brak interpunkcji frazowej w stabilnej części ("home." poza LCP)
        assert!(tr
            .on_partial(1, "I think it's red, and then we go home.")
            .is_none());
        let frag = tr
            .on_partial(1, "I think it's red, and then we go home.")
            .expect("dwa zgodne przebiegi w nowej formie");
        // emisja MUSI zacząć się od "and" — pierwszego niewyemitowanego słowa
        assert_eq!(frag.text, "and then we go home.");
        tr.commit(&frag);
        let emit = tr.on_final(1, 1, 1, "I think it's red, and then we go home.");
        assert!(!emit.reanchored);
        assert_eq!(emit.text, ""); // final w całości pokryty fragmentami
    }

    // T15: skumulowany dryf licznika ponad ANCHOR_K — okno ±k chybia, ale
    // przeszukanie CAŁEJ listy finalu ratuje ogon (stary fallback c−k
    // wypadał poza listę i gubił go w całości)
    #[test]
    fn t15_dryf_ponad_k_pelna_lista() {
        let mut tr = tracker();
        let base = "one two three four five six seven eight nine ten eleven twelve,";
        assert!(tr.on_partial(1, base).is_none());
        let frag = tr
            .on_partial(1, &format!("{base} tail"))
            .expect("commit 12 słów");
        assert_eq!(frag.new_committed, 12);
        tr.commit(&frag);
        // final widzi scommitowaną treść tylko jako 4 słowa na początku —
        // pozycja kotwicy (0) daleko poza oknem [base−k, base+k]
        let emit = tr.on_final(1, 1, 1, "nine ten eleven twelve, real tail here.");
        assert!(!emit.reanchored);
        assert_eq!(emit.text, "real tail here.");
        assert_eq!(tr.reanchors, 0);
    }

    // T16: final SKLEJKI niosący nowszą gen niż commity trackera (typowy
    // przypadek koalescencji pod zaległością) — zakres [gen_oldest, gen]
    // traktowany jako to samo audio: scommitowane słowa deduplikowane
    // kotwicą zamiast re-emisji hurtem
    #[test]
    fn t16_final_sklejki_nowszej_gen_deduplikuje() {
        let mut tr = committed_abcd(5);
        let emit = tr.on_final(
            6,
            5,
            2,
            "alpha bravo charlie delta, echo fox. next utterance here.",
        );
        assert!(!emit.reanchored);
        assert!(emit.had_commits);
        assert_eq!(emit.text, "echo fox. next utterance here.");
        assert!(tr.is_stale(6));
    }

    // T16b: final generacji SPOZA zakresu sklejki — commity dotyczyły innego
    // audio: pełna emisja, strata ogona policzona
    #[test]
    fn t16b_final_spoza_zakresu_pelna_emisja() {
        let mut tr = committed_abcd(5);
        let emit = tr.on_final(7, 7, 1, "totally new segment text.");
        assert!(!emit.had_commits);
        assert_eq!(emit.text, "totally new segment text.");
        assert_eq!(tr.lost_tails, 1);
    }

    // T17: has_commits — tani odczyt dla bramek stt_thread, honoruje zakres
    // generacji sklejki
    #[test]
    fn t17_has_commits_zakres() {
        let tr = tracker();
        assert!(!tr.has_commits(1, 1));
        let tr = committed_abcd(5);
        assert!(tr.has_commits(5, 5));
        assert!(tr.has_commits(4, 6)); // sklejka obejmująca gen 5
        assert!(!tr.has_commits(6, 7)); // commity spoza zakresu
    }

    // T18: hipoteza całkowicie przepisana po commicie — kotwica nie do
    // odnalezienia: ŻADNEJ emisji pozycyjnej (mogłaby przeskoczyć prawdziwe
    // słowa); ogon załatwia final (tu: fallback licznikowy c−k z duplikatem)
    #[test]
    fn t18_rewizja_bez_kotwicy_wstrzymuje_emisje() {
        let mut tr = committed_abcd(2);
        assert!(tr.on_partial(2, "totally different words here, more").is_none());
        assert!(tr
            .on_partial(2, "totally different words here, more still")
            .is_none());
        let emit = tr.on_final(2, 2, 1, "totally different words here, more still.");
        assert!(emit.reanchored);
        // start = committed(4) − k(3) = 1 → duplikat, nigdy ubytek
        assert_eq!(emit.text, "different words here, more still.");
    }

    // T19: REGRESJA Z AUDYTU (#69, 20 s przed końcem filmu). Whisper wstawia
    // słowo PRZED granicą commitu ("see more of" → "see more of it"), przez co
    // anchor_exact_at chybia, a okno przesunięte o jedno słowo ma odległość
    // edycyjną dokładnie 2 = tol dla kotwicy 4-słowowej. PRZED poprawką pętla
    // rozmyta dopasowywała ["it","in","the","review"] do kotwicy
    // ["below","in","the","comments"] i cofała licznik 10 → 7, więc krok D
    // emitował "below in the comments. I probably will post this on Reddit."
    // — dokładnie duplikat z logu (09:13:44.556, ta sama fraza po raz trzeci
    // w 4 sekundy). Przebieg tol=0 znajduje kotwicę na przesuniętej pozycji.
    #[test]
    fn t19_rewizja_w_prawo_nie_cofa_licznika() {
        let mut tr = tracker();
        assert!(tr
            .on_partial(1, "see more of in the review below in the comments. I")
            .is_none());
        let frag = tr
            .on_partial(1, "see more of in the review below in the comments. I probably")
            .expect("stabilny prefiks cięty na kropce");
        assert_eq!(frag.text, "see more of in the review below in the comments.");
        assert_eq!(frag.new_committed, 10);
        tr.commit(&frag); // kotwica = [below, in, the, comments]

        // rewizja: whisper dokleja "it" przed granicą commitu; LCP=3 <=
        // committed, więc ten przebieg i tak nic nie emituje — liczy się
        // wyłącznie skutek uboczny na liczniku
        let revised =
            "see more of it in the review below in the comments. I probably will post this on Reddit. So";
        assert!(tr.on_partial(1, revised).is_none());
        assert_eq!(tr.resyncs_up, 1);
        assert_eq!(tr.resyncs_down, 0);

        let frag = tr
            .on_partial(1, revised)
            .expect("dwa zgodne przebiegi w nowej formie");
        assert_eq!(frag.text, "I probably will post this on Reddit.");
        assert_eq!(tr.resyncs_up, 1);
        assert_eq!(tr.resyncs_down, 0);
    }

    // T20: REGRESJA PRZECIW NAIWNEJ WERSJI REKOMENDACJI 3 — literalny
    // kontrprzykład z audytu. Ścieżka PEŁNOLISTOWA (coalesced=2 ⇒
    // counter_valid=false) MUSI dalej wybierać NAJWCZEŚNIEJSZE okno poniżej
    // progu: prawdziwe miejsce ma d=2 przy w=1, a identyczna fraza dalej
    // ("all the way up again") daje dopasowanie IDEALNE przy w=11. Gdyby
    // przebieg tol=0 zastosować globalnie, emisja ruszyłaby od "again ok."
    // — UBYTEK 8 słów, całe zdanie znika bezpowrotnie.
    // Duplikat 2 słów jest TAŃSZY niż ubytek 8 — to jest cena, którą świadomie
    // płacimy na ścieżce pełnolistowej.
    #[test]
    fn t20_pelna_lista_finalu_nadal_najwczesniejsze_okno() {
        let mut tr = tracker();
        assert!(tr.on_partial(1, "well I said all the way up, then").is_none());
        let frag = tr
            .on_partial(1, "well I said all the way up, then we")
            .expect("fragment cięty na przecinku");
        assert_eq!(frag.text, "well I said all the way up,");
        tr.commit(&frag); // kotwica = [all, the, way, up]

        let emit = tr.on_final(
            1,
            1,
            2,
            "i said all the way on up to here and then all the way up again ok.",
        );
        assert!(!emit.reanchored);
        assert_eq!(emit.text, "on up to here and then all the way up again ok.");
    }

    // T21: REGRESJA Z AUDYTU (#43 "ventilation"). Whisper dokleił z przodu "a"
    // (log: '#43 szew: zdjęto powtórzone "a" z nakładki'), indeksy przesunęły
    // się o 1 i okno licznikowe ±k w on_final dopasowywało kotwicę o jedną
    // pozycję za wcześnie (d=2 = tol), przez co ogon finalu zaczynał się od
    // ostatniego JUŻ WYEMITOWANEGO słowa. PRZED poprawką: "delta, echo fox."
    #[test]
    fn t21_ogon_finalu_bez_duplikatu_po_szwie() {
        let mut tr = committed_abcd(1);
        let emit = tr.on_final(1, 1, 1, "zero alpha bravo charlie delta, echo fox.");
        assert!(!emit.reanchored);
        assert!(emit.had_commits);
        assert_eq!(emit.text, "echo fox.");
        assert_eq!(tr.reanchors, 0);
    }

    // T22: REGRESJA PRZECIW PRZEBIEGOWI IDEALNEMU NA KRÓTKIEJ KOTWICY.
    // Segment otwarty fragmentem 2-słowowym (bramka przepuszcza go po liczbie
    // znaków) zostawia kotwicę m=2, a wtedy warunkiem przeskoku przestaje być
    // powtórzony 4-gram i staje się powtórzony BI-GRAM. Tu mówca powtarza
    // "absolutely fantastic", a whisper rewiduje PIERWSZE wystąpienie
    // ("Absolutely" → "Absolute"). Bez bramki m >= ANCHOR_M przebieg idealny
    // dopasowywał kotwicę do DRUGIEGO wystąpienia i ogon finalu zaczynał się od
    // "work here." — dwa słowa, których lektor nigdy nie powiedział, znikały
    // bezpowrotnie. Poprawny wynik to duplikat: pętla rozmyta (tol=1 przy m<4)
    // trafia w prawdziwe, zrewidowane miejsce z d=1. Duplikat jest CENĄ,
    // nie błędem — zasada nadrzędna pliku.
    #[test]
    fn t22_krotka_kotwica_powtorzona_fraza_bez_ubytku() {
        let mut tr = tracker();
        assert!(tr
            .on_partial(1, "Absolutely fantastic, absolutely")
            .is_none());
        let frag = tr
            .on_partial(1, "Absolutely fantastic, absolutely fantastic")
            .expect("fragment 2-słowowy przechodzi bramkę po liczbie znaków");
        assert_eq!(frag.text, "Absolutely fantastic,");
        assert_eq!(frag.new_committed, 2);
        tr.commit(&frag); // kotwica = [absolutely, fantastic], m = 2

        let emit = tr.on_final(1, 1, 1, "Absolute fantastic, absolutely fantastic work here.");
        assert!(!emit.reanchored);
        assert_eq!(emit.text, "absolutely fantastic work here.");
    }
}
