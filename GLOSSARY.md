# git-weld

`git-weld` is a semantic three-way Git merge driver that isolates line conflicts into syntax-aware chunks and asks a System One decision model whether either side alone already preserves both changes.

## Language

### Merge Inputs and Structure

**Version**:
One of the three inputs to a three-way merge: **base**, **ours**, or **theirs**.
_Avoid_: File, revision, branch

**Base**:
The common ancestor version from which **ours** and **theirs** diverged.
_Avoid_: Ancestor, original

**Side**:
One of the two diverging versions being merged: **ours** or **theirs**, excluding **base**.
_Avoid_: Branch, head, parent

**Conflict**:
A region where **ours** and **theirs** make overlapping, non-identical line edits that a line merge cannot combine on its own.
_Avoid_: Collision, merge conflict, hunk

**Item**:
A named syntactic declaration in one **version** of a file (such as a function, method, struct, enum, trait, impl block, module, or class), including its leading comments, attributes, and decorators.
_Avoid_: Node, symbol, declaration

**Diverged item**:
An **item** that both **sides** changed differently relative to **base**, whether or not the edits overlap by line.
_Avoid_: AST conflict, item conflict

**Chunk**:
A contiguous span of merge regions widened to cover whole **items** in all three **versions**, judged and replaced as a single unit.
_Avoid_: Conflict hunk, AST chunk

### Semantic State and Decisions

**Semantic state**:
The neutral, structured description of a conflicted file's **chunks** and their supporting context sent to a **provider**.
_Avoid_: Prompt, payload

**Provider**:
A backend that evaluates the **semantic state** of a file and returns one **decision** per **chunk**.
_Avoid_: Engine, adapter, client

**Configured model**:
The model identifier requested by the user in configuration before a **provider** call is made.
_Avoid_: Model, target model

**Answering model**:
The specific model version reported by a **provider** in its response after evaluating a request.
_Avoid_: Model, resolved model

**Decision**:
A **provider**'s judgment for a single **chunk**, consisting of an action (`TakeOurs`, `TakeTheirs`, or `Manual`), a calibrated confidence from 0 to 1, and a reason.
_Avoid_: Verdict, resolution, answer

**Manual provider**:
The built-in **provider** mode (`manual`) that skips model evaluation and leaves every **conflict** for **manual resolution**.
_Avoid_: Bare "manual"

**Manual decision**:
The **decision** action (`Manual`) indicating that neither **side** alone is complete and a person must merge the **chunk**.
_Avoid_: Bare "manual"

**Manual resolution**:
Leaving standard conflict markers in the merged file for a person to edit.
_Avoid_: Bare "manual", fallback mode

**Resolved**:
A **chunk** replaced by one **side**'s code because its **decision** chose that **side** above the confidence threshold and the stitched file parsed cleanly.
_Avoid_: Auto-merged, stitched

**Hoisted signature**:
The declaration header and member signatures of an **item** referenced by a **chunk**, with executable bodies omitted, included in the **semantic state** as supporting context.
_Avoid_: Context node, stub, skeleton

**Decision cache**:
The repository-local store in the shared Git directory that maps a hash of a **chunk**, its **hoisted signatures**, the **provider**, and the **configured model** to a previously recorded **decision**.
_Avoid_: Merge cache, rerere cache

**Decision log**:
The append-only JSON Lines file (`.git-weld.log`) in the working tree that records each **chunk**'s **decision**, line range, resolution status, and whether it came from the **decision cache**.
_Avoid_: Audit log, trace file

