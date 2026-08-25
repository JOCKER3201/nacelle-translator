# Budowanie i instalacja nacelle-translator (feature "cuda" — wymaga CUDA
# Toolkit w środowisku budowania, np. distrobox "Fedora"; patrz README).
#
#   make build           ./build.sh (czysty build, cargo clean + release)
#   make dist             pakuje binarkę + biblioteki CUDA runtime do dist/
#   sudo make install     kopiuje dist/ + models/ do /opt/nacelle-translator
#                         (bin/, lib/, models/); usuwa ./target przed i po
#                         instalacji (zasada czystego builda)
#   sudo make uninstall
#   make clean
#
# `install` celowo NIE zależy od `build`/`dist` i nigdy nie woła cargo —
# kompilacja jako root pod sudo mieszałaby właściciela plików w target/ i
# mogłaby nie widzieć CUDA Toolkit spoza PATH roota. Zbuduj i spakuj jako
# zwykły użytkownik (w distroboxie), instaluj przez sudo tylko gotowe pliki.

PREFIX ?= /opt/nacelle-translator

.PHONY: build dist install uninstall clean

build:
	./build.sh

dist:
	./package.sh

install:
	rm -rf target
	@test -f dist/bin/nacelle-translator || { \
		echo "błąd: brak dist/bin/nacelle-translator — najpierw: make build && make dist" >&2; \
		exit 1; \
	}
	install -d "$(DESTDIR)$(PREFIX)/bin" "$(DESTDIR)$(PREFIX)/lib" "$(DESTDIR)$(PREFIX)/models"
	install -m 755 dist/bin/* "$(DESTDIR)$(PREFIX)/bin/"
	install -m 644 dist/lib/* "$(DESTDIR)$(PREFIX)/lib/"
	@if ls models/* >/dev/null 2>&1; then \
		install -m 644 models/* "$(DESTDIR)$(PREFIX)/models/"; \
	else \
		echo "uwaga: brak plików w models/ — pomijam instalację modeli" >&2; \
	fi
	install -m 644 dist/nacelle-translator.toml.example dist/README.md "$(DESTDIR)$(PREFIX)/"
	rm -rf target
	@echo "zainstalowano do $(DESTDIR)$(PREFIX) (bin/ + lib/ + models/)"
	@echo "uruchamiaj z katalogu instalacji, żeby względne ścieżki modeli z configu działały:"
	@echo "  cd $(PREFIX) && ./bin/translator          # cały stos jednym poleceniem"

uninstall:
	rm -rf "$(DESTDIR)$(PREFIX)"

clean:
	cargo clean
	rm -rf dist
