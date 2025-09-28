# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

PetalSonic is a real-time safe spatial audio library for Rust that uses Steam Audio (audionimbus) for 3D spatialization. It provides a world-driven API where the main thread owns and updates a 3D world (listener + sources), while fixed-size audio processing threads handle spatialization and playback.

## Workspace Architecture

This project uses a **workspace structure** to separate the core library from demo/example code:

```
petalsonic/
├── Cargo.toml              # Workspace manifest
├── petalsonic-core/        # Pure audio library
│   ├── Cargo.toml
│   └── src/                # Core library modules
└── petalsonic-demo/        # Demo applications and examples
    ├── Cargo.toml
    └── src/main.rs         # CLI demo and tests
```

### Core Library (`petalsonic-core`)

**Purpose**: Pure spatial audio processing library with no UI dependencies
**Contains**: Audio engine, world management, spatialization, data loading
**Dependencies**: Only audio-related crates (cpal, audionimbus, symphonia, etc.)

### Demo Crate (`petalsonic-demo`)

**Purpose**: Examples, tests, and future interactive applications
**Contains**: CLI demos, integration tests, future web UI components
**Dependencies**: Core library + UI frameworks when needed

## Common Development Commands

### Build and Test

```bash
# Build entire workspace
cargo build

# Run demo application
cargo run

# Run clippy on workspace
cargo clippy
```
