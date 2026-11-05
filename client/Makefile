MAKEFLAGS = --no-print-directory --always-make --silent
MAKE = make $(MAKEFLAGS)

# Set DEV_SLOT=1, DEV_SLOT=2, etc. to run multiple instances on different ports
DEV_SLOT ?= 0

# Prerelease channel for `make publish`; empty publishes the release as `latest`.
DIST_TAG ?=

dev:
	@echo "Booting up frontend...$(if $(filter-out 0,$(DEV_SLOT)), (slot $(DEV_SLOT)),)"
	PORT=$$((3000 + $(DEV_SLOT) * 100)) \
	SB_PORT=$$((4000 + $(DEV_SLOT) * 100)) \
	SB_EXPRESS_PORT=$$((3005 + $(DEV_SLOT) * 100)) \
	NEXT_PUBLIC_LOCAL_SERVER_PORT=$$((8888 + $(DEV_SLOT) * 1000)) \
	NEXT_PUBLIC_FEEDBACK_API_URI=http://localhost:$$((8888 + $(DEV_SLOT) * 1000)) \
	pnpm run dev --ui tui $(filter-out $@,$(MAKECMDGOALS))

lint:
	pnpm run check-format

lint-fix:
	pnpm run check-format --write

# Publishes every kept package to npm as @backendrelay/*, versioned from
# packages/version/src/version.ts. scripts/publish_packages.mjs stages a renamed
# copy of each package, so the working tree keeps its @instantdb/* names.
# Needs npm credentials for the scope; see docs/sdk-publishing.md.
publish:
	@echo "Building + Publishing @backendrelay/* packages..."
	pnpm build-packages
	node ./scripts/publish_packages.mjs $(DIST_TAG)

# Same pipeline with `npm publish --dry-run`: no upload, but npm lists every file
# that the tarball would contain.
publish-dry-run:
	@echo "Building + dry-running the @backendrelay/* publish..."
	pnpm build-packages
	node ./scripts/publish_packages.mjs $(DIST_TAG) --dry-run

publish-experimental:
	@echo "(Experimental Build) Building + Publishing Packages..."
	pnpm build-packages
	node ./scripts/publish_packages.mjs experimental
