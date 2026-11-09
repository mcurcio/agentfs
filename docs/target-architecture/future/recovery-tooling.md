# Recovery tooling beyond restore and revert

**Proposed, not built.** This page proposes more tools for recovering from a bad change. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

Recovery uses `restore`, `revert` and the runbooks ([recovery and undo](../../runbooks/recovery-and-undo.md)).

## What it proposes

Two questions, each answered only as real use shows the need:

- Is a **read-only history view for operators** worth building?
- Should **undo go through the patch queue**, replacing manual restores for infrastructure-as-code?
