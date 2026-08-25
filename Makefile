# Budowanie i instalacja nacelle-translator (feature "cuda" — wymaga CUDA
# Toolkit w środowisku budowania, np. distrobox "Fedora"; patrz README).
#
#   make build      ./build.sh (czysty build, cargo clean + release) — sam build
#   make dist       pakuje binarkę + biblioteki CUDA runtime do dist/ — samo pakowanie
#   make install    JEDNO polecenie: czysty build + pakowanie + instalacja do
#                   /opt/nacelle-translator (bin/, lib/, models/, config). Usuwa
#                   ./target przed budowaniem i zaraz po spakowaniu, więc zawsze
#                   instaluje świeżo zbudowaną wersję, nigdy nieaktualne dist/.
#   make uninstall
#   make clean
#
# WAŻNE: uruchamiaj jako "make install", NIE "sudo make install" — cargo/build.sh
# mają iść jako Ty (sudo dla całości myliłoby właściciela plików w target/ i
# mogłoby nie widzieć CUDA Toolkit spoza PATH roota). sudo jest wywoływane
# WEWNĄTRZ reguły install, tylko dla samego kopiowania do /opt — dostaniesz
# jedno pytanie o hasło w trakcie.

PREFIX ?= /opt/nacelle-translator

.PHONY: build dist install uninstall clean

build:
	./build.sh

dist:
	./package.sh

install:
	rm -rf target
	./build.sh --fast
	./package.sh
	rm -rf target
	@test -f dist/bin/nacelle-translator || { \
		echo "błąd: dist/bin/nacelle-translator nie powstał — sprawdź log budowania wyżej" >&2; \
		exit 1; \
	}
	sudo install -d "$(DESTDIR)$(PREFIX)/bin" "$(DESTDIR)$(PREFIX)/lib" "$(DESTDIR)$(PREFIX)/models"
	sudo install -m 755 dist/bin/* "$(DESTDIR)$(PREFIX)/bin/"
	sudo install -m 644 dist/lib/* "$(DESTDIR)$(PREFIX)/lib/"
	@if ls models/* >/dev/null 2>&1; then \
		sudo install -m 644 models/* "$(DESTDIR)$(PREFIX)/models/"; \
	else \
		echo "uwaga: brak plików w models/ — pomijam instalację modeli" >&2; \
	fi
	sudo install -m 644 dist/nacelle-translator.toml.example dist/README.md "$(DESTDIR)$(PREFIX)/"
	@if [ -f dist/nacelle-translator.toml ]; then \
		sudo install -m 644 dist/nacelle-translator.toml "$(DESTDIR)$(PREFIX)/"; \
	else \
		echo "uwaga: brak lokalnego nacelle-translator.toml — zainstalowano tylko .example" \
			"(bez configu binarka użyje domyślnego silnika \"gemini\" i zażąda GEMINI_API_KEY)" >&2; \
	fi
	@echo "zainstalowano do $(DESTDIR)$(PREFIX) (bin/ + lib/ + models/)"
	@echo "uruchamiaj z katalogu instalacji:"
	@echo "  cd $(PREFIX) && ./bin/translator"

uninstall:
	sudo rm -rf "$(DESTDIR)$(PREFIX)"

clean:
	cargo clean
	rm -rf dist
