#!/bin/bash

set -ex

cargo build --release
spin-debug pluginify
spin-debug plugins install -f trigger-remote.json --yes
