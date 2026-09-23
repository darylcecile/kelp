# Sources and evidence notes

[Overview](README.md) · [Research synthesis](01-research.md)

**Research checked:** 23 September 2026. Links to living documentation may change. Publication dates are included where available; dates of old case studies are not claims about today's production scale.

The proposal's schemas, CLI, architecture, performance targets, and roadmap are design recommendations. They are not reported capabilities of an existing Kelp implementation. Primary documentation supports mechanism claims; polls and observations identify hypotheses to validate.

## Git internals and scaling

### G1

**Git project — [Git's core data model](https://git-scm.com/docs/gitdatamodel).** Official manual, introduced/updated in Git 2.53.0; accessed as living documentation.

Supports immutable object types, snapshot representation, refs, `HEAD`, index stages, and local reflogs. Particularly useful for correcting the misconception that commits are stored text diffs or full physical copies of every file.

### G2

**Git project — [Git wire protocol v2](https://git-scm.com/docs/gitprotocol-v2).** Official living specification; retrieved page includes Git 2.55.0 updates.

Supports stateless command handling, selective reference discovery, fetch negotiation, object-format negotiation, packfile URIs, bundle URIs, and promisor-remote advertisement. Explicitly contradicts a blanket claim that Git cannot load-balance or use external content distribution.

### G3

**Git project — [Partial clone design](https://git-scm.com/docs/partial-clone).** Official design notes.

Supports lazy object retrieval, promisor remotes, multiple providers, and offline limitations. Some implementation-specific limitation notes are historical; the proposal relies on the documented concepts, not an assertion that every listed performance limitation applies to every current command.

### G4

**Taylor Blau / GitHub — [Scaling monorepo maintenance](https://github.blog/open-source/git/scaling-monorepo-maintenance/).** 29 April 2021; updated 30 June 2022.

Production engineering report on repacking, object reachability, bitmaps, and incremental maintenance. Evidence that storage maintenance and traversal costs matter—and that Git has substantial optimizations. Not a present-day GitHub service benchmark.

### G5

**Derrick Stolee / GitHub — [Git's database internals V: scalability](https://github.blog/open-source/git/gits-database-internals-v-scalability/).** 2 September 2022.

Explores component, submodule, time-based, and data-offloading strategies. Supports the distinction between application/project boundaries and physical scaling choices. Its observations about future tooling are time-bound.

### G6

**Microsoft — [Scalar repository and migration notice](https://github.com/microsoft/scalar).** Historical project README, accessed during research.

Lists partial clone, background prefetch, sparse checkout, filesystem monitoring, commit-graph, multi-pack-index, and incremental repack. The repository explicitly says Scalar moved; use it as background on the scaling techniques, not as current installation guidance.

### G7

**Git project — [git-push](https://git-scm.com/docs/git-push) and [pack protocol](https://git-scm.com/docs/gitprotocol-pack).** Official manuals, also queried through Context7's `/git/htmldocs` documentation index.

Supports reference update requests containing old/new IDs and server-supported atomic multi-ref pushes. Kelp is not claiming atomic publication was absent from Git.

### G8

**Git LFS project — [Git Large File Storage](https://git-lfs.com/).** Official project overview.

Documents pointer files in Git and separately stored large-file payloads. Supports the proposal to unify the user-visible lifecycle; it does not imply LFS cannot scale or share access controls with its host.

## User experience evidence

### U1

**Julia Evans — [Some Git poll results](https://jvns.ca/blog/2024/03/28/git-poll-results/).** 28 March 2024.

Contains response counts, questions, terminology confusion, self-reported work loss, and preferences. The author explicitly identifies selection and wording limitations. These are self-selected social-media polls, not a representative survey or measured data-loss study.

### U2

**Julia Evans — [Notes on Git's error messages](https://jvns.ca/blog/2024/04/10/notes-on-git-error-messages/) and [New zine: How Git Works!](https://jvns.ca/blog/2024/04/25/new-zine--how-git-works-/).** April 2024.

Concrete examples of ambiguous errors, divergent branches, last-known remote state, and operation-dependent conflict labels. Qualitative teaching experience; not evidence that every Git user has these problems.

### U3

**Santiago Perez De Rosso and Daniel Jackson — [Gitless research project](https://sdg.csail.mit.edu/project/gitless).** Links to *What's Wrong with Git? A Conceptual Design Analysis* (2013) and [*Purposes, Concepts, Misfits, and a Redesign of Git*](https://spderosso.github.io/oopsla16.pdf) (OOPSLA 2016).

Conceptual analysis, Stack Overflow question analysis, and a small user study. Useful for examining mismatches between purpose and exposed state. The age and limited study scope prevent generalizing its results to all modern Git workflows.

## Successors and related models

### A1

**Jujutsu project — [Comparison with Git](https://docs.jj-vcs.dev/latest/git-comparison/) and [Glossary](https://docs.jj-vcs.dev/latest/glossary/).** Official living documentation.

Documents stable change IDs across rewrites, the working-copy model, absence of a user-facing index, independent visible heads, automatic descendant rebasing, and conflicts. Establishes that substantial workflow improvement is already possible with Git interoperability.

### A2

**Jujutsu project — [Operation log](https://docs.jj-vcs.dev/latest/operation-log/).** Official living documentation.

Describes immutable operation/view records, undo/restore, and divergent operations. Direct inspiration for recoverable workspace operations; Kelp's exact retention and overwrite rules are proposed separately.

### A3

**Sapling project — [Working at scale: overview](https://sapling-scm.com/docs/scale/overview/).** Official living documentation.

Describes working-set-oriented optimizations, lazy history, EdenFS, and the explicit move from a distributed Mercurial foundation toward a client-server architecture. Supports both the potential gains and the offline/independence tradeoff.

### A4

**Meta / Sapling — Mononoke [documentation index](https://github.com/facebook/sapling/blob/main/eden/mononoke/docs/README.md), [storage architecture](https://github.com/facebook/sapling/blob/main/eden/mononoke/docs/2.4-storage-architecture.md), [Bonsai data model](https://github.com/facebook/sapling/blob/main/eden/mononoke/docs/2.1-bonsai-data-model.md), and [pushrebase](https://github.com/facebook/sapling/blob/main/eden/mononoke/docs/4.1-pushrebase.md).** Official repository documentation, read from raw source.

Supports separate immutable blobs and mutable metadata, external storage, stateless services, chunked contents, derived indexes, and serialized bookmark updates with off-path computation. The project distinguishes Meta production use from externally supported deployment; open-source availability does not imply turnkey operations.

### A5

**Pijul project — [Theory](https://pijul.org/manual/theory.html) and [Conflicts](https://pijul.org/manual/conflicts.html).** Official manual.

Explains change dependencies, line/file identities, graph-based conflict representation, and why a CRDT can converge while representing unresolved source conflicts. Performance and correctness claims made by the project are not independently benchmarked in this research.

### A6

**Jujutsu project — [First-class conflicts](https://docs.jj-vcs.dev/latest/conflicts/) and [technical conflict model](https://docs.jj-vcs.dev/latest/technical/conflicts/).** Official living documentation.

Shows that a conflict can be retained in versioned data rather than forcing an operation to remain suspended. Kelp borrows the product principle without claiming its proposed merge representation is identical to Jujutsu's algebra.

## Hosting, monorepos, and peer distribution

### D1

**GitLab — [Gitaly Cluster / Praefect](https://docs.gitlab.com/administration/gitaly/praefect/).** Official living administration documentation.

Documents replicated repositories, distributed reads, consistency behavior, and the specific risk that lagging replicas push read traffic back onto a heavily updated primary. Distinguish this implementation's limitations from universal limits of Git.

### D2

**Rachel Potvin and Josh Levenberg — [Why Google Stores Billions of Lines of Code in a Single Repository](https://research.google/pubs/why-google-stores-billions-of-lines-of-code-in-a-single-repository/).** Communications of the ACM, 2016; [full article](https://cacm.acm.org/research/why-google-stores-billions-of-lines-of-code-in-a-single-repository/).

Describes Piper, CitC, distributed infrastructure, and small workspace overlays over a large shared repository. Reported codebase figures are historical, not current estimates. Evidence of feasibility in Google's environment, not a universal product benchmark.

### D3

**Radicle project — [Protocol guide](https://radicle.network/guides/protocol).** Official living documentation.

Documents signed repository identity, delegates, peer announcements, selective replication, and Git-based data transfer. Useful precedent for provider independence and portable collaboration. It does not establish horizontal partitioning of a single project's native object store.

## What this research does not establish

- Market demand for a new incompatible VCS rather than a better Git/Jujutsu/backend integration.
- Measured performance, cost, merge correctness, or durability of the proposed Kelp design.
- Representative prevalence of individual Git complaints.
- Current operating scale of every cited production system.

The [delivery and validation plan](06-delivery-and-validation.md) turns those gaps into explicit experiments and release gates.
