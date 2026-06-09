set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

ubuntu_packages := "build-essential curl file libayatana-appindicator3-dev libdbus-1-dev librsvg2-dev libssl-dev libwebkit2gtk-4.1-dev pkg-config xdotool"

default:
    @just --list

install:
    pnpm install --frozen-lockfile

ubuntu-deps:
    sudo apt update
    sudo apt install -y {{ubuntu_packages}}

build:
    pnpm build

test:
    pnpm build
    cd src-tauri && cargo test

deb:
    pnpm install --frozen-lockfile
    pnpm tauri build --bundles deb

ubuntu-deb: ubuntu-deps deb

install-deb:
    sudo apt install ./src-tauri/target/release/bundle/deb/*.deb
