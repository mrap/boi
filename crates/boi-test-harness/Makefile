COMPOSE := docker compose -f docker/docker-compose.yaml

.PHONY: up down clean e2e e2e-up e2e-down logs

up:
	$(COMPOSE) up -d etcd

down:
	$(COMPOSE) down -v

clean: down
	rm -rf ../../e2e-artifacts

e2e-up:
	$(COMPOSE) up -d
	./docker/etcd-readiness.sh

e2e-down:
	$(COMPOSE) down -v

logs:
	$(COMPOSE) logs --no-color

# Run the full red-baseline suite. `--test-threads=1` keeps docker
# compose state hermetic per test (no shared topology between tests).
# Forward $(ARGS) so callers can do `make e2e ARGS="--filter <name>"`.
e2e:
	cargo test -p boi-test-harness --features e2e -- --test-threads=1 $(ARGS)
