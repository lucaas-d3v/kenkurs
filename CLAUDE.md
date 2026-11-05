# Kenku FM Native (Rust Rewrite)

## Objetivos do Projeto
- Reescrever o Kenku FM (TypeScript/Electron) em Rust puro.
- Foco em baixíssimo consumo de memória RAM e inicialização instantânea.
- Sem tecnologias web (zero Electron, zero HTML/CSS/JS, zero WebViews).
- Áudio nativo usando bibliotecas Rust (ex: rodio, cpal).

## Estrutura de Referência
- `kenku-fm/`: Repositório oficial em Electron para consulta da lógica de áudio e rotas
  (player em `src/player/`, API remota em `src/main/remote/`, Discord em `src/main/broadcast/`).
- `src/`: Código-fonte do aplicativo nativo (crate `kenkurs`).

## Arquitetura (`src/`)
- `audio/`: motor de áudio. `decoder` (symphonia) → `resample` (sinc polifásico) → `stream`
  (thread do motor: decodifica ~0,5 s à frente, loop sem emenda, crossfade de loop, fades) →
  `voice`/`mixer` (callback de tempo real: sem alocação, sem locks, sem I/O) → `output`
  (cpal, ou "relógio silencioso" quando não há dispositivo).
- `state/`: dados puros. Biblioteca, fila com shuffle/repeat, configurações, persistência JSON atômica.
- `app/`: `Controller` (porta de `usePlaylistPlayback`/`useSoundboardPlayback` e dos handlers
  remotos) e `runtime::Core` (threads, eventos do motor, autosave).
- `main.rs`: executor headless (`kenkurs play|sound|devices|info`) até a GUI egui existir.

## Ambiente de desenvolvimento (aarch64, proot no Termux)
- O `cargo` do PATH é o do Termux (alvo `aarch64-linux-android`) e não serve aqui. Use o
  toolchain glibc: `export PATH=$HOME/rust/build/aarch64-unknown-linux-gnu/stage0/bin:$PATH`.
- Não há ALSA nem `/dev/snd` no proot: `cargo test --no-default-features` (usa o relógio silencioso).
- Type-check do backend cpal sem ALSA: `PKG_CONFIG_PATH=$PWD/dev/pkgconfig cargo check --all-targets`.
- Produção (Void Linux x86_64): `sudo xbps-install -S alsa-lib-devel` e `cargo build --release`.
