# 🛡️ ArchGuard

**The Blazing-Fast, Language-Agnostic Architectural Linter.**

ArchGuard is a zero-copy static analysis tool built in **Rust** that enforces strict architectural boundaries in your codebase. By analyzing your code's abstract syntax tree (AST) via Tree-sitter, it ensures that your dependency rules (e.g., "The UI layer cannot directly call the Database layer") are never violated. 

In the era of AI-assisted coding and rapid development, ArchGuard acts as your automated software architect, preventing structural degradation (software entropy) and keeping your codebase modular and maintainable.

## ✨ Features

- **🚀 Blazing Fast:** Built in Rust with a zero-copy pipeline. Analyzes thousands of files in milliseconds.
- **🌍 Language-Agnostic Core:** Uses a universal `DependencyEdge` data model. Currently supports Python out-of-the-box, with an extensible architecture ready for any language via Tree-sitter.
- **🤖 The Ultimate AI Guardrail:** Prevents AI coding assistants from taking shortcuts that destroy your moduler architecture.
- **⚡️ CI/CD Ready:** Designed natively for GitHub Actions to automatically reject architectural violations on Pull Requests.

## 🚀 Quick Start

### 1. Define Your Architecture
Create an `archguard.yaml` file in the root of your project to define your layers and rules:

read DSL document for refrence 

2. Run Locally
Run ArchGuard in your terminal. It will instantly map all dependencies and flag any violations:

archguard check --config archguard.yaml


3. 🐙 GitHub Actions Integration
Prevent architectural violations before they merge. Add ArchGuard to your CI pipeline by creating a .github/workflows/archguard.yml file:

name: ArchGuard Check

on:
  pull_request:
    branches: [ main, master ]

jobs:
  architecture-check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      
      - name: Run ArchGuard
        uses: sultan462/archGuard@v1
        with:
          config: 'archguard.yaml'
(Note: If ArchGuard finds a violation, it will automatically fail the CI run and point out exactly which file broke the architectural rules).

🧠 How It Works (The Architecture)
Unlike dynamic analysis tools or regex-based linters, ArchGuard deeply understands your code.

It uses Tree-sitter to parse source code into a static AST.

Isolated language parsers (e.g., src/Languages/python.rs) extract import statements 

These are mapped to a universal DependencyEdge graph.

The graph is evaluated against your YAML rules in microseconds.

Because language parsing is strictly isolated from the core engine, adding support for a new language (like TypeScript, Go, or C++) requires zero changes to the core validation logic.

🤝 Contributing
Contributions are welcome! Whether it's adding a new language parser, improving the documentation, or fixing a bug, feel free to open an issue or submit a Pull Request.

📄 License
This project is licensed under the MIT License.