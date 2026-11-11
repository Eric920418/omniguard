# OmniGuard 地端閘道。編譯期對齊專案工具鏈，執行期只留二進位與憑證。
FROM rust:1.99.1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 omniguard
COPY --from=build /src/target/release/omniguard /usr/local/bin/omniguard
USER omniguard
EXPOSE 7843
ENV BIND=0.0.0.0:7843
ENTRYPOINT ["omniguard"]
