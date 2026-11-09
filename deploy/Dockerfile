# Optional: run the whole stack (supervisor + all services) in one container.
#   build the interface first:  (cd frontend && npm install && npm run build)
#   then, from the project root:  docker compose -f deploy/docker-compose.yml up --build
FROM rust:1.91-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang libclang-dev pkg-config libssl-dev libasound2-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --workspace --exclude e2e \
 && mkdir -p /out && cp target/release/{supervisor,gateway,orchestrator,llm-service,stt-service,tts-service,memory-service,persona-service,avatar-service} /out/ \
 && (find target/release/build ~/.cache -name '*.so*' 2>/dev/null | grep -E 'onnxruntime|sherpa-onnx' | xargs -r -I{} cp -n {} /out/ || true)

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl bzip2 libstdc++6 libgomp1 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /out/ /app/target/release/
COPY config ./config
COPY frontend/dist ./frontend/dist
COPY scripts ./scripts
COPY assets ./assets
# inside the container the gateway must listen on all interfaces; Ollama runs in its own container
ENV LD_LIBRARY_PATH=/app/target/release WAIFU_ADDR_GATEWAY=0.0.0.0:8080 WAIFU_OLLAMA_URL=http://ollama:11434
EXPOSE 8080
CMD ["/bin/sh", "-c", "./scripts/download-models.sh --small; exec ./target/release/supervisor"]
