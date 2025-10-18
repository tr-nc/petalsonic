#!/bin/bash
set -e  # Exit on error

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Get current version from Cargo.toml
cd petalsonic
VERSION=$(grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')
cd ..

echo -e "${GREEN}Publishing petalsonic v$VERSION${NC}"
echo ""

# Step 1: Format code
echo -e "${YELLOW}[1/4] Running cargo fmt...${NC}"
if ! cargo fmt --all -- --check; then
    echo -e "${RED}Code is not formatted. Running cargo fmt to fix...${NC}"
    cargo fmt --all
    echo -e "${YELLOW}Code has been formatted. Please review changes and run again.${NC}"
    exit 1
fi
echo -e "${GREEN}✓ Code formatting OK${NC}"
echo ""

# Step 2: Check compilation
echo -e "${YELLOW}[2/4] Running cargo check...${NC}"
if ! cargo check --all-targets; then
    echo -e "${RED}✗ cargo check failed!${NC}"
    echo -e "${RED}Please fix compilation errors before publishing.${NC}"
    exit 1
fi
echo -e "${GREEN}✓ Compilation check passed${NC}"
echo ""

# Step 3: Run clippy
echo -e "${YELLOW}[3/4] Running cargo clippy...${NC}"
if ! cargo clippy --all-targets -- -D warnings; then
    echo -e "${RED}✗ cargo clippy found issues!${NC}"
    echo -e "${RED}Please fix all warnings and errors before publishing.${NC}"
    exit 1
fi
echo -e "${GREEN}✓ Clippy checks passed${NC}"
echo ""

# Step 4: Publish
echo -e "${YELLOW}[4/4] Publishing petalsonic crate...${NC}"
cd petalsonic

echo -e "${GREEN}Publishing petalsonic $VERSION to crates.io...${NC}"
cargo publish

cd ..
echo ""
echo -e "${GREEN}================================${NC}"
echo -e "${GREEN}✓ Successfully published v$VERSION!${NC}"
echo -e "${GREEN}================================${NC}"
