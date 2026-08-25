# Budowanie i instalacja nacelle-translator (feature "cuda" — wymaga CUDA
# Toolkit w środowisku budowania, np. distrobox "Fedora"; patrz README).
#
#   make build           ./build.sh (czysty build, cargo clean + release)
#   make dist             pakuje binarkę + biblioteki CUDA runtime do dist/
#   sudo make install     kopiuje dist/ do /opt/nacelle-translator (bez budowania)
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
	@test -f dist/bin/nacelle-translator || { \
		echo "błąd: brak dist/bin/nacelle-translator — najpierw: make build && make dist" >&2; \
		exit 1; \
	}
	install -d "$(DESTDIR)$(PREFIX)/bin" "$(DESTDIR)$(PREFIX)/lib"
	install -m 755 dist/bin/* "$(DESTDIR)$(PREFIX)/bin/"
	install -m 644 dist/lib/* "$(DESTDIR)$(PREFIX)/lib/"
	install -m 644 dist/nacelle-translator.toml.example dist/README.md "$(DESTDIR)$(PREFIX)/"
	@echo "zainstalowano do $(DESTDIR)$(PREFIX)"
	@echo "uruchamiaj: $(PREFIX)/bin/nacelle-translator"

uninstall:
	rm -rf "$(DESTDIR)$(PREFIX)"

clean:
	cargo clean
	rm -rf dist
