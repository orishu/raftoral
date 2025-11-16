#!/bin/bash
set -e

echo "Building Raftoral for WebAssembly..."
echo ""

# Check if wasm-bindgen-cli is installed
if ! command -v wasm-bindgen &> /dev/null; then
    echo "Error: wasm-bindgen-cli is not installed."
    echo "Install it with: cargo install wasm-bindgen-cli"
    exit 1
fi

# Check if wasm32-unknown-unknown target is installed
if ! rustup target list --installed | grep -q "wasm32-unknown-unknown"; then
    echo "Installing wasm32-unknown-unknown target..."
    rustup target add wasm32-unknown-unknown
fi

echo "Step 1: Building for wasm32-unknown-unknown target..."
echo "  - Using --no-default-features to exclude RocksDB (not WASM-compatible)"
echo "  - Using --release for optimized build"
echo "  - WASM config flags set in .cargo/config.toml"
echo ""

cargo build --target wasm32-unknown-unknown --release --no-default-features

echo ""
echo "Step 2: Generating JavaScript bindings with wasm-bindgen..."
echo "  - Output directory: pkg/"
echo "  - Target: web (for browser usage)"
echo ""

# Create pkg directory if it doesn't exist
mkdir -p pkg

# Generate JS bindings
wasm-bindgen target/wasm32-unknown-unknown/release/raftoral.wasm \
    --out-dir pkg \
    --target web \
    --typescript

echo ""
echo "✅ WASM build complete!"
echo ""
echo "Output files:"
echo "  - pkg/raftoral.js         (JavaScript bindings)"
echo "  - pkg/raftoral_bg.wasm    (WebAssembly binary)"
echo "  - pkg/raftoral.d.ts       (TypeScript definitions)"
echo ""
echo "To use in a web application:"
echo "  import init, { WasmRaftoralNode } from './pkg/raftoral.js';"
echo "  await init();"
echo "  const node = new WasmRaftoralNode(1, '127.0.0.1:8080', true);"
echo "  await node.start();"
echo ""
