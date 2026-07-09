.PHONY: help install dev fetcher server web test test-server test-fetcher lint build up down logs

SCRATCH ?= ./scratch
export FEEDBOT_DB      ?= $(SCRATCH)/feedbot.db
export FEEDBOT_STATIC  ?= web/dist
export FEEDBOT_PORT    ?= 8099
# Local dev runs the sidecar in its own terminal, so don't let the server spawn one.
export FEEDBOT_FETCHER_SCRIPT ?=

help:
	@grep -E '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) | sed 's/:.*## /\t/'

install: ## Install node deps for the sidecar and the web app
	cd fetcher && npm install
	cd web && npm install

fetcher: ## Run the Playwright sidecar on :4000 (needs `npx playwright install chromium` once)
	cd fetcher && npm start

server: ## Run the Rust server on :8099 against a scratch db
	@mkdir -p $(SCRATCH)
	cd server && cargo run

web: ## Run vite on :5173, proxying /api to :8099
	cd web && npm run dev

build: ## Build the release container
	docker compose build

up: ## Bring the container up on the shared `web` network
	docker compose up -d

down:
	docker compose down

logs:
	docker compose logs -f feedbot

test: test-server test-fetcher ## Run every test

test-server: ## Unit tests: url policy, db, epub, auth
	cd server && cargo test

test-fetcher: ## Live tests: hits the real seed blogs; needs the sidecar running
	cd fetcher && npm test

lint:
	cd server && cargo clippy --all-targets -- -D warnings && cargo fmt --check
