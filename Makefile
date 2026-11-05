.PHONY: clean clean-artifacts clean-deps prune-dev

clean: clean-deps clean-artifacts
	@echo "Cleanup complete."

clean-deps:
	@echo "Removing local dependency folders..."
	@rm -rf client/node_modules client/www/node_modules
	@echo "Removed client/node_modules and client/www/node_modules."

clean-artifacts:
	@echo "Removing local build artifacts..."
	@rm -rf client/.turbo server-rs/target
	@echo "Removed client/.turbo and server-rs/target."

prune-dev:
	@echo "Pruning local build caches (when available)..."
	@if command -v pnpm >/dev/null 2>&1; then \
		echo "Running pnpm store prune in client..."; \
		(cd client && pnpm store prune); \
	else \
		echo "pnpm not found, skipping pnpm store prune."; \
	fi
	@if command -v cargo >/dev/null 2>&1; then \
		echo "Running cargo clean in server-rs..."; \
		(cd server-rs && cargo clean); \
	else \
		echo "cargo not found, skipping cargo clean."; \
	fi

.PHONY: verify-quick verify-local verify-dashboard-browser verify-security-policy audit
verify-quick:
	python3 self-hosting/verify-local.py --profile quick

verify-local:
	python3 self-hosting/verify-local.py --profile local

verify-dashboard-browser:
	bash self-hosting/run-dashboard-browser-acceptance.sh

verify-security-policy:
	python3 self-hosting/security/production-policy.py

audit:
	@command -v cargo-deny >/dev/null 2>&1 || { \
		echo "cargo-deny is required: cargo install --locked cargo-deny"; \
		exit 1; \
	}
	(cd server-rs && cargo deny check advisories licenses sources)
	@command -v pnpm >/dev/null 2>&1 || { \
		echo "pnpm is required: corepack enable pnpm"; \
		exit 1; \
	}
	(cd client && pnpm audit --prod)
