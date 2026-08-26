//! Opcje eksperymentalne wiersza poleceń: `--experimental-futures=a,b,c`.
//!
//! Flaga jest JEDYNYM wejściem do funkcji eksperymentalnych — celowo nie ma
//! dla nich kluczy w pliku konfiguracyjnym. Inaczej „eksperymentalne" znaczy
//! tylko „domyślnie wyłączone": raz wpisany klucz w TOML zostaje na zawsze
//! i po miesiącu nikt nie pamięta, czy dana sesja szła torem zwykłym czy
//! eksperymentalnym. Flaga w wierszu poleceń jest widoczna w `ps`, w historii
//! powłoki i w logu startowym, więc przebieg da się jednoznacznie odtworzyć.
//!
//! DODANIE KOLEJNEJ OPCJI = JEDEN wpis w tablicy [`FEATURES`] i nic więcej:
//! nazwa, opis (leci do `--help` i do podkomendy `check`) oraz domknięcie
//! przestawiające pole w [`Config`]. Parser, walidacja, `--help`, `check`
//! i log startowy czytają tę tablicę, więc żaden z nich nie wymaga zmiany.
//! Reszta programu (pipeline) widzi wyłącznie gotową konfigurację i nie wie
//! nawet, że flaga istnieje.

use crate::config::Config;

/// Nazwa flagi w jednym miejscu — używana też w komunikatach błędów.
pub const FLAG: &str = "--experimental-futures";

pub struct Feature {
    /// nazwa podawana po przecinku w `--experimental-futures=`
    pub name: &'static str,
    /// jedno zdanie do `--help` i do `check`
    pub help: &'static str,
    /// włączenie w JUŻ WCZYTANEJ konfiguracji — dzięki temu tor AI czyta
    /// dalej tylko `Config` i nie musi znać pojęcia „opcja eksperymentalna"
    pub enable: fn(&mut Config),
}

/// JEDYNE miejsce, w którym istnieje lista opcji eksperymentalnych.
pub const FEATURES: &[Feature] = &[Feature {
    name: "speculative-stt",
    help: "spekulacyjne STT (LocalAgreement-2): stabilny prefiks otwartego segmentu \
           idzie do tłumaczenia, zanim VAD go domknie — niższe opóźnienie \
           pierwszych słów kosztem dodatkowych przebiegów whispera",
    enable: |cfg| cfg.stt.speculative = true,
}];

/// Zbiór opcji włączonych flagą — w kolejności pierwszego wystąpienia.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// indeksy w [`FEATURES`]; indeks zamiast nazwy, żeby „włączona" i
    /// „istniejąca" opcja były z definicji tym samym
    enabled: Vec<usize>,
}

impl Selection {
    /// Dokłada opcje z jednego wystąpienia flagi (wartość bez nazwy flagi).
    ///
    /// Powtórzenie flagi SUMUJE zbiory zamiast być błędem: opcje i tak są
    /// zbiorem, a nakładanie się argumentów z aliasu/skryptu na argumenty
    /// dopisane ręcznie w terminalu to normalny sposób pracy — błąd zmuszałby
    /// do sklejania listy w jednym miejscu, czyli do edycji cudzego skryptu.
    /// Duplikat tej samej nazwy też przechodzi (idempotencja włączania).
    pub fn extend_from_spec(&mut self, spec: &str) -> Result<(), String> {
        if spec.trim().is_empty() {
            return Err(format!(
                "{FLAG} wymaga listy opcji oddzielonych przecinkami (np. {FLAG}={})",
                FEATURES[0].name
            ));
        }
        for raw in spec.split(',') {
            // Białe znaki wokół nazw ucinamy: żadna nazwa opcji ich nie
            // zawiera, więc „a, b" po zacytowaniu w powłoce jest jednoznaczne.
            let name = raw.trim();
            // Pusty element ("a,,b", "a,") to literówka, a nie prośba
            // o cokolwiek — po cichu zignorowany zamieniłby się w milczące
            // „opcja niewłączona, bo przecinek stanął nie tam".
            if name.is_empty() {
                return Err(format!("{FLAG}: pusta nazwa opcji (zbędny przecinek?)"));
            }
            match FEATURES.iter().position(|f| f.name == name) {
                Some(i) => {
                    if !self.enabled.contains(&i) {
                        self.enabled.push(i);
                    }
                }
                None => return Err(format!("{FLAG}: nieznana opcja \"{name}\"")),
            }
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.enabled.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &'static Feature> + '_ {
        self.enabled.iter().map(|&i| &FEATURES[i])
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.iter().map(|f| f.name).collect()
    }

    /// Nakłada włączone opcje na wczytaną konfigurację.
    pub fn apply(&self, cfg: &mut Config) {
        for f in self.iter() {
            (f.enable)(cfg);
        }
    }

    /// Linia INFO na starcie — w logu z sesji musi być widać gołym okiem,
    /// czy przebieg szedł torem eksperymentalnym (dwa warianty tego samego
    /// materiału inaczej są nie do odróżnienia po samych logach).
    pub fn log_startup(&self) {
        if self.is_empty() {
            return;
        }
        for f in self.iter() {
            log::info!("OPCJA EKSPERYMENTALNA: {} — {}", f.name, f.help);
        }
    }
}

/// Blok pomocy z listą opcji — dla `--help` i dla komunikatów błędów.
/// Generowany z [`FEATURES`], żeby dopisanie opcji nie wymagało ruszania
/// stałej USAGE w main.rs.
pub fn help_block() -> String {
    let example = FEATURES.iter().map(|f| f.name).collect::<Vec<_>>().join(",");
    let mut s = String::new();
    s.push_str(&format!("  {FLAG}=LISTA\n"));
    s.push_str("                  włącz opcje eksperymentalne — LISTA to nazwy oddzielone\n");
    s.push_str(&format!("                  PRZECINKAMI, np. {FLAG}={example}\n"));
    s.push_str(&format!("                  (działa też \"{FLAG} LISTA\"; powtórzenie\n"));
    s.push_str("                  flagi sumuje opcje). Dostępne opcje:\n");
    for f in FEATURES {
        s.push_str(&format!("                    {}\n", f.name));
        s.push_str(&wrap_help(f.help));
    }
    s
}

/// Zawija opis opcji do ~96 kolumn, z wcięciem pod nazwą opcji.
fn wrap_help(help: &str) -> String {
    const WIDTH: usize = 70;
    const INDENT: &str = "                        ";
    let mut out = String::from(INDENT);
    let mut line = 0usize;
    for word in help.split_whitespace() {
        if line > 0 && line + 1 + word.len() > WIDTH {
            out.push('\n');
            out.push_str(INDENT);
            line = 0;
        } else if line > 0 {
            out.push(' ');
            line += 1;
        }
        out.push_str(word);
        line += word.chars().count();
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brak_flagi_nic_nie_wlacza() {
        let sel = Selection::default();
        assert!(sel.is_empty());
        let mut cfg = Config::default();
        sel.apply(&mut cfg);
        assert!(!cfg.stt.speculative, "bez flagi spekulacja musi zostać wyłączona");
    }

    #[test]
    fn poprawna_nazwa_wlacza_opcje() {
        let mut sel = Selection::default();
        sel.extend_from_spec("speculative-stt").unwrap();
        assert_eq!(sel.names(), vec!["speculative-stt"]);
        let mut cfg = Config::default();
        sel.apply(&mut cfg);
        assert!(cfg.stt.speculative);
    }

    #[test]
    fn lista_po_przecinku_akceptuje_wszystkie_znane_nazwy() {
        // lista wszystkich zadeklarowanych opcji naraz musi się parsować —
        // test pilnuje, że nowo dopisana opcja nie psuje składni listy
        let spec = FEATURES.iter().map(|f| f.name).collect::<Vec<_>>().join(",");
        let mut sel = Selection::default();
        sel.extend_from_spec(&spec).unwrap();
        assert_eq!(sel.names().len(), FEATURES.len());
    }

    #[test]
    fn nieznana_nazwa_to_blad() {
        let mut sel = Selection::default();
        let e = sel.extend_from_spec("speculative-stt,cos-innego").unwrap_err();
        assert!(e.contains("cos-innego"), "komunikat musi cytować złą nazwę: {e}");
        // stan po błędzie nie ma znaczenia — main kończy proces kodem 2,
        // ale lista znanych opcji musi być gdzie ją wypisać
        assert!(help_block().contains("speculative-stt"));
    }

    #[test]
    fn puste_wartosci_miedzy_przecinkami_to_blad() {
        for spec in ["speculative-stt,,speculative-stt", "speculative-stt,", ",speculative-stt"] {
            let mut sel = Selection::default();
            let e = sel.extend_from_spec(spec).unwrap_err();
            assert!(e.contains("pusta nazwa"), "spec {spec:?} dało: {e}");
        }
    }

    #[test]
    fn pusta_wartosc_flagi_to_blad() {
        let mut sel = Selection::default();
        assert!(sel.extend_from_spec("").is_err());
        assert!(sel.extend_from_spec("   ").is_err());
    }

    #[test]
    fn biale_znaki_wokol_nazw_sa_ucinane() {
        let mut sel = Selection::default();
        sel.extend_from_spec("  speculative-stt  ").unwrap();
        assert_eq!(sel.names(), vec!["speculative-stt"]);
    }

    #[test]
    fn powtorzona_flaga_sumuje_a_duplikat_nie_dubluje() {
        let mut sel = Selection::default();
        sel.extend_from_spec("speculative-stt").unwrap();
        sel.extend_from_spec("speculative-stt").unwrap();
        assert_eq!(sel.names(), vec!["speculative-stt"], "duplikat nie może się powielić");
    }

    #[test]
    fn nazwy_opcji_sa_unikalne_i_bez_przecinkow() {
        // niezmiennik tablicy FEATURES: przecinek w nazwie albo duplikat
        // uczyniłyby opcję nieosiągalną z wiersza poleceń
        for (i, f) in FEATURES.iter().enumerate() {
            assert!(!f.name.contains(','), "nazwa {:?} zawiera przecinek", f.name);
            assert!(!f.name.trim().is_empty());
            assert!(
                FEATURES.iter().skip(i + 1).all(|g| g.name != f.name),
                "zduplikowana nazwa opcji: {:?}",
                f.name
            );
        }
    }
}
