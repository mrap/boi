# Root proxy Makefile. Forwards distributed-BOI E2E targets to the
# harness crate so contributors run `make e2e` from the repo root.

HARNESS := crates/boi-test-harness

.PHONY: e2e e2e-up e2e-down e2e-clean e2e-logs

e2e:
	$(MAKE) -C $(HARNESS) e2e ARGS="$(ARGS)"

e2e-up:
	$(MAKE) -C $(HARNESS) e2e-up

e2e-down:
	$(MAKE) -C $(HARNESS) e2e-down

e2e-clean:
	$(MAKE) -C $(HARNESS) clean

e2e-logs:
	$(MAKE) -C $(HARNESS) logs
