# Refusing shared mappings

**Proposed, not built.** This page proposes refusing shared writable memory mappings on the tree views. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

Shared mappings work, with one recorded gap: a mapped store that writes the bytes its page last held records
nothing, so a descriptor write made meanwhile stands where Linux would keep the store. A store not yet written back
dies with the view host, which Linux would keep. `stat` and `sendfile` show unsaved state.

## What it proposes

Refuse `MAP_SHARED` writable mappings, so the gap cannot occur. The code that handles page fills and page writes is
in `tree_actor/recording/pages.rs` and the view's read, write and size paths, so removing it is cheap.

## Open choices

Whether the programs that matter need shared writable mappings. The proposal waits for a case where the gap bites.
