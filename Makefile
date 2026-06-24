# Optimimer — build + deploy the backend to a Raspberry Pi (or any aarch64 box) over SSH.
#
#   make deploy PI=pi@192.168.1.50   # cross-build, ship binary+agents, run interactive installer
#   make redeploy PI=pi@...          # ship binary+agents + restart — reuses existing config, no prompts
#   make build-pi                    # just cross-compile the static aarch64 binary
#   make logs    PI=pi@...           # follow the service logs
#   make restart PI=pi@...           # restart the service
#   make stop    PI=pi@...           # stop the service
#   make uninstall PI=pi@...         # remove the service + install dir from the Pi
#
# SSH/SCP/rsync force IPv4 by default (SSHFLAGS=-4) — some networks need it.
# Override if you don't:  make deploy PI=pi@host SSHFLAGS=
#
# The bot uses long-polling, so the Pi needs NO open ports and NO public IP.
# A prebuilt binary ships in deploy/bin, so `make deploy` works even without a
# Rust toolchain locally.

PI         ?=
SSHFLAGS   ?= -4
REMOTE_DIR := /opt/optimimer
TRIPLE     := aarch64-unknown-linux-musl
BIN        := backend/target/$(TRIPLE)/release/optimimer-backend
PREBUILT   := deploy/bin/optimimer-backend-aarch64

# Set INFISICAL_ENV=<slug> to pull secrets from YOUR LOCAL infisical at deploy
# time and inject them into the remote installer — the Pi needs no infisical CLI.
INFISICAL_ENV ?=

SSH   := ssh $(SSHFLAGS)
SCP   := scp $(SSHFLAGS)
RSYNC := rsync -az --delete -e 'ssh $(SSHFLAGS)'

.DEFAULT_GOAL := help
.PHONY: help build-pi require-pi deploy redeploy logs restart stop uninstall

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

build-pi: ## Cross-compile the static aarch64 binary (needs cargo-zigbuild + zig)
	cd backend && cargo zigbuild --release --target $(TRIPLE)
	cp $(BIN) $(PREBUILT)
	@echo "Built $(PREBUILT)"

require-pi:
	@test -n "$(PI)" || { echo "Set PI=user@host  e.g.  make deploy PI=pi@192.168.1.50"; exit 1; }

deploy: require-pi ## Ship to the Pi + run installer (add INFISICAL_ENV=dev to inject local secrets)
	@if command -v cargo >/dev/null 2>&1; then $(MAKE) build-pi; \
	 else echo "cargo not found locally — shipping prebuilt $(PREBUILT)"; fi
	@SRC="$(BIN)"; [ -f "$$SRC" ] || SRC="$(PREBUILT)"; \
	 echo "==> shipping $$SRC to $(PI):$(REMOTE_DIR)"; \
	 $(SSH) $(PI) "sudo mkdir -p $(REMOTE_DIR) && sudo chown \$$(id -un):\$$(id -gn) $(REMOTE_DIR)"; \
	 $(SCP) "$$SRC" $(PI):$(REMOTE_DIR)/optimimer-backend.new; \
	 $(SSH) $(PI) "chmod +x $(REMOTE_DIR)/optimimer-backend.new && mv -f $(REMOTE_DIR)/optimimer-backend.new $(REMOTE_DIR)/optimimer-backend"; \
	 $(RSYNC) backend/agents/ $(PI):$(REMOTE_DIR)/agents/; \
	 $(SCP) deploy/install.sh $(PI):$(REMOTE_DIR)/install.sh; \
	 if [ -n "$(INFISICAL_ENV)" ]; then \
	   command -v infisical >/dev/null 2>&1 || { echo "infisical CLI not found locally — install it or omit INFISICAL_ENV"; exit 1; }; \
	   echo "==> exporting secrets from local infisical (env=$(INFISICAL_ENV)) and injecting on the Pi"; \
	   umask 077; infisical export --env=$(INFISICAL_ENV) --format=dotenv > .opti.env.tmp; \
	   $(SCP) .opti.env.tmp $(PI):/tmp/opti.env; rm -f .opti.env.tmp; \
	   $(SSH) -t $(PI) "set -a; . /tmp/opti.env; set +a; rm -f /tmp/opti.env; chmod +x $(REMOTE_DIR)/install.sh; $(REMOTE_DIR)/install.sh"; \
	 else \
	   $(SSH) -t $(PI) "chmod +x $(REMOTE_DIR)/install.sh && $(REMOTE_DIR)/install.sh"; \
	 fi

redeploy: require-pi ## Ship binary+agents + restart — reuses existing config, NO installer/prompts
	@if command -v cargo >/dev/null 2>&1; then $(MAKE) build-pi; \
	 else echo "cargo not found locally — shipping prebuilt $(PREBUILT)"; fi
	@SRC="$(BIN)"; [ -f "$$SRC" ] || SRC="$(PREBUILT)"; \
	 echo "==> shipping $$SRC to $(PI):$(REMOTE_DIR) (config untouched)"; \
	 $(SSH) $(PI) "sudo mkdir -p $(REMOTE_DIR) && sudo chown \$$(id -un):\$$(id -gn) $(REMOTE_DIR)"; \
	 $(SCP) "$$SRC" $(PI):$(REMOTE_DIR)/optimimer-backend.new; \
	 $(RSYNC) backend/agents/ $(PI):$(REMOTE_DIR)/agents/; \
	 $(SSH) $(PI) "chmod +x $(REMOTE_DIR)/optimimer-backend.new && mv -f $(REMOTE_DIR)/optimimer-backend.new $(REMOTE_DIR)/optimimer-backend && sudo systemctl restart optimimer && sudo systemctl --no-pager status optimimer | head -5"

logs: require-pi ## Follow the service logs on the Pi
	$(SSH) -t $(PI) "journalctl -u optimimer -f"

restart: require-pi ## Restart the service
	$(SSH) $(PI) "sudo systemctl restart optimimer && sudo systemctl --no-pager status optimimer | head -5"

stop: require-pi ## Stop the service
	$(SSH) $(PI) "sudo systemctl stop optimimer"

uninstall: require-pi ## Stop, disable and remove the service + install dir
	$(SSH) $(PI) "sudo systemctl disable --now optimimer 2>/dev/null; \
		sudo rm -f /etc/systemd/system/optimimer.service; \
		sudo systemctl daemon-reload; sudo rm -rf $(REMOTE_DIR)"
