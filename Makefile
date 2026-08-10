.PHONY: build build-arm64 test vet fmt clean eval-mock

# Native build (whatever host you're on).
build:
	go build -o dist/piforge ./cmd/piforge
	go build -o dist/piforge-eval ./cmd/piforge-eval

# Cross-compile for the Raspberry Pi 5 target (static, no CGo).
build-arm64:
	CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build -ldflags="-s -w" -o dist/piforge-arm64 ./cmd/piforge
	CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build -ldflags="-s -w" -o dist/piforge-eval-arm64 ./cmd/piforge-eval

test:
	go test -race ./...

vet:
	go vet ./...

fmt:
	gofmt -w internal/ cmd/

# Headless eval run that exercises the runner + sim + broker + scorer without a
# llama-server. Uses scripted mock turns for the seed cases.
eval-mock:
	go build -o dist/piforge-eval ./cmd/piforge-eval
	./dist/piforge-eval --mock --config piforge.toml.example

clean:
	rm -rf dist/
