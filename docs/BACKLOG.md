# Hamlet Future Work & Post-1.0 Backlog

This document tracks planned architectural extensions, performance optimizations, and feature enhancements for Hamlet following the 1.0 release milestone.

---

## 1. Multi-Host Village Clustering & Federation

- **Objective**: Extend Hamlet from a single Linux workstation to distributed multi-host clusters.
- **Description**:
  - Distribute agent parcels across multiple physical nodes while preserving the Singular Governor principle.
  - Implement a Raft-based consensus protocol to replicate the village charter, commission records, and decree history across cluster nodes.
  - Mutual TLS (mTLS) for cross-node freight synchronization and peer mail delivery.
  - Remote container execution via rootless Podman socket bridging.

---

## 2. Operator Identity Federation & RBAC for Web Interface

- **Objective**: Provide enterprise-grade authentication and access control for the Hamlet Web UI.
- **Description**:
  - Integrate OpenID Connect (OIDC) and OAuth 2.0 authentication providers (e.g., GitHub, Google, Keycloak).
  - Define Role-Based Access Control (RBAC):
    - **Observer**: Read-only access to village state, telemetry, and commission progress.
    - **Commissioner**: Authorized to post new commissions and review Git diffs.
    - **Mayor Administrator**: Authorized to review escalations, approve vault promotions, and override decrees.
  - Audit logging of all operator actions to a tamper-evident log in `keep/telemetry.sled/`.

---

## 3. Hardware Accelerator & GPU Passthrough for Local Inference

- **Objective**: Enable local containerized AI models to access host GPU acceleration.
- **Description**:
  - Leverage Podman Container Device Interface (CDI) to expose host GPUs (NVIDIA CUDA, AMD ROCm, Intel oneAPI) to designated parcel containers.
  - Support containerized local inference engines (Ollama, llama.cpp, vLLM) running inside agent parcels.
  - Enforce VRAM and GPU compute limits per parcel to prevent resource starvation.

---

## 4. Dynamic cgroup v2 Resource Throttling & Adaptive Hibernation

- **Objective**: Adapt agent resource bounds in real time based on host pressure and commission urgency.
- **Description**:
  - Extend the hibernation policy (ADR-0040) with active cgroup v2 memory pressure monitoring (`memory.pressure`).
  - Dynamically scale parcel CPU quotas (`cpu.max`) and memory limits (`memory.high` / `memory.max`) without requiring container restarts.
  - Predictive hibernation of idle agents when system memory approaches saturation thresholds.

---

## 5. Extended Sandboxed Package Managers & Language Runtimes

- **Objective**: Broaden the set of pinned and verified developer toolchains.
- **Description**:
  - Add native support for additional package managers and build tools:
    - OCaml (`opam`)
    - Ruby (`bundler`)
    - Java / Kotlin (`maven`, `gradle`)
    - Go (`go` modules cache mirror)
    - Haskell (`ghcup`, `cabal`)
  - Maintain the immutable mirror trust model (ADR-0013): every downloaded binary and archive must match a concrete sha256 pin in `guild/pins/`.

---

## 6. Hierarchical Mayor Context Packet Distillation

- **Objective**: Optimize context window usage for long-running and high-turnover commissions.
- **Description**:
  - Implement recursive summarization of prior decree cycles, git diff history, and mail exchanges into bounded context packets (ADR-0027).
  - Provide semantic search and retrieval over archived village telemetry and mail spools.

---

## 7. Interactive Terminal Streaming in Web UI

- **Objective**: Enable real-time operator inspection of live agent container sessions in the browser.
- **Description**:
  - Embed `xterm.js` in the Web UI dashboard connected via WebSocket to daemon session spools.
  - Support read-only streaming of agent stdout/stderr with optional authorized interactive takeover.
