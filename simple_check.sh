#!/bin/bash
cd /Users/sw/heart-portal
export RUST_BACKTRACE=1
cargo check --message-format=short 2>&1 | head -100