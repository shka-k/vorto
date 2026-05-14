PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
BIN    := vorto
TARGET := target/release/$(BIN)

.PHONY: all build install uninstall clean

all: build

build:
	cargo build --release

$(TARGET): build

install: $(TARGET)
	install -d $(DESTDIR)$(BINDIR)
	install -m 755 $(TARGET) $(DESTDIR)$(BINDIR)/$(BIN)

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/$(BIN)

clean:
	cargo clean
