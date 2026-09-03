IMAGE ?= toad-computer:next
NAME ?= toad-computer-next
PORT ?= 8787

.PHONY: check image run stop contract logs

check:
	cargo fmt --all --check
	cargo clippy --all-targets -- -D warnings
	cargo test

image:
	docker build -t $(IMAGE) .

# A fresh token per run, kept in .token for `make contract`. The container
# runs with every capability dropped: nothing in it is root.
run: stop
	openssl rand -hex 24 > .token
	docker run -d --name $(NAME) \
	  --cap-drop=ALL --security-opt no-new-privileges \
	  --pids-limit 512 --memory 2g --shm-size 1g \
	  -p 127.0.0.1:$(PORT):8787 \
	  -e TOAD_COMPUTER_TOKEN="$$(cat .token)" \
	  $(IMAGE)

stop:
	-docker rm -f $(NAME) >/dev/null 2>&1

# The contract test drives the running container through a real MCP client.
contract:
	TOAD_COMPUTER_URL=http://127.0.0.1:$(PORT) TOAD_COMPUTER_TOKEN="$$(cat .token)" \
	  cargo test --test contract -- --nocapture

logs:
	docker logs $(NAME)
