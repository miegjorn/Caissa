REGISTRY  := ghcr.io/miegjorn
SHA       := $(shell git rev-parse --short HEAD)
FONDAMENT := ../Fondament
AMASSADA  := ../Amassada

# ── auth ─────────────────────────────────────────────────────────────────────
.PHONY: login
login:
	gh auth token | docker login ghcr.io -u miegjorn --password-stdin

# ── guilhem ──────────────────────────────────────────────────────────────────
# Builds the guilhem sandbox image via the caissa CLI (which reads Fondament
# definitions and assembles the Dockerfile), then pushes and updates the chart.

.PHONY: guilhem guilhem-build guilhem-push guilhem-deploy

guilhem-build:
	cargo run --bin caissa -- build guilhem --fondament-path $(FONDAMENT)

guilhem-push: login guilhem-build
	docker tag caissa-sandbox:guilhem $(REGISTRY)/caissa-sandbox:guilhem
	docker tag caissa-sandbox:guilhem $(REGISTRY)/caissa-sandbox:guilhem-sha-$(SHA)
	docker push $(REGISTRY)/caissa-sandbox:guilhem
	docker push $(REGISTRY)/caissa-sandbox:guilhem-sha-$(SHA)

guilhem-deploy: guilhem-push
	sed -i '' \
	  "s|image: $(REGISTRY)/caissa-sandbox:guilhem-sha-.*|image: $(REGISTRY)/caissa-sandbox:guilhem-sha-$(SHA)|" \
	  deploy/charts/guilhem/values.yaml \
	  deploy/charts/component-agents/values.yaml
	git add deploy/charts/guilhem/values.yaml deploy/charts/component-agents/values.yaml
	git diff --staged --quiet || \
	  git commit -m "chore(guilhem): deploy guilhem-sha-$(SHA) [skip ci]"
	git push

guilhem: guilhem-deploy

# ── amassada ─────────────────────────────────────────────────────────────────
# amassada-core has a path dep on fondament-core, so the build needs Fondament
# as a named build context (mirrors what CI does with fondament-src checkout).

.PHONY: amassada amassada-push amassada-deploy

amassada-push: login
	docker buildx build \
	  --build-context fondament=$(FONDAMENT) \
	  -t $(REGISTRY)/amassada:latest \
	  -t $(REGISTRY)/amassada:sha-$(SHA) \
	  --push \
	  $(AMASSADA)

amassada-deploy: amassada-push
	yq -i '.amassada.imageTag = "sha-$(SHA)"' deploy/charts/occitan/values.yaml
	git add deploy/charts/occitan/values.yaml
	git diff --staged --quiet || \
	  git commit -m "chore(amassada): deploy sha-$(SHA) [skip ci]"
	git push

amassada: amassada-deploy

# ── fondament server ─────────────────────────────────────────────────────────
.PHONY: fondament fondament-push fondament-deploy

fondament-push: login
	docker buildx build \
	  -t $(REGISTRY)/fondament:latest \
	  -t $(REGISTRY)/fondament:sha-$(shell cd $(FONDAMENT) && git rev-parse --short HEAD) \
	  --push \
	  $(FONDAMENT)

fondament-deploy: fondament-push
	@FTAG=sha-$$(cd $(FONDAMENT) && git rev-parse --short HEAD); \
	yq -i ".fondament.imageTag = \"$$FTAG\"" deploy/charts/occitan/values.yaml; \
	git add deploy/charts/occitan/values.yaml; \
	git diff --staged --quiet || \
	  git commit -m "chore(fondament): deploy $$FTAG [skip ci]"; \
	git push

fondament: fondament-deploy

# ── all ───────────────────────────────────────────────────────────────────────
.PHONY: all
all: guilhem amassada
