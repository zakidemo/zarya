# Contributing to Zarya

Thank you for your interest. Zarya is an early research prototype, so the most useful contributions right now are:

- **Attempts to break it**: a program whose label misses a value that reaches the output, a way to read an undeclared resource, or a way around an output contract. Please report security issues privately (see [SECURITY.md](SECURITY.md)).
- **Bug reports** with the smallest program that shows the problem, the command you ran, and what you expected.
- **Questions and ideas** in GitHub issues.

## Before you open a pull request

1. Open an issue first for anything larger than a small fix, so we can agree on the approach.
2. Build and test:

   ```bash
   cd runtime/rust
   cargo test --release --offline
   cd ../..
   python -I -X utf8 label_check/marker_check.py runtime/rust/target/release/zarya
   python -I -X utf8 run_demo.py
   ```

3. Keep the runtime free of `unsafe` code and external dependencies.
4. Add a test for every behaviour you change.

If you used AI tools to write part of a contribution, say so in the pull request, and make sure you understand and can explain every line.

## Language

Issues and pull requests are welcome in English or Arabic.
