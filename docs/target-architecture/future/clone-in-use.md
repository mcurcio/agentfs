# Cloning a claim that is in use

**Proposed, not built.** This page proposes handling a CSI clone whose source claim a pod is using. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

Kubernetes' documentation says a clone's source claim must be bound and available, not in use. The
external-provisioner version the driver pins checks only that the source is bound, not being deleted, of the same
driver and no larger than the new claim. On a kind cluster a fork class cloned a claim a running pod writes to.
A later provisioner or Kubernetes release may add the check the documentation describes ([CSI](../../csi.md)).

## What it proposes

Watch later releases, and run the kind job that clones a claim in use. If a release adds the check, the driver's
fork path gets another way to start from a running volume's head.
