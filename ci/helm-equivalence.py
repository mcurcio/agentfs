#!/usr/bin/env python3
"""Compares two streams of Kubernetes objects: the kustomize render of a
deploy/ directory (expected) and the helm render of its chart (actual).
Used by ci/helm-equivalence.sh (docs/helm.md §5).

Only what Helm adds by design is ignored:
  - the release's own Namespace object in the expected side (Helm installs
    into a namespace that already exists, docs/helm.md);
  - Helm's standard labels (helm.sh/chart, app.kubernetes.io/instance,
    managed-by, version, part-of) and `checksum/*` annotations;
  - kustomize's content-hash suffix on the names given with --unhash;
  - with --object-namespace, the namespace of an expected object that has
    none (a plain manifest, applied with `kubectl -n`).
Everything else, every field and value, must be equal.

Usage: helm-equivalence.py EXPECTED ACTUAL [--namespace NS] [--unhash NAME]...
                           [--object-namespace NS]
Exit status 0 when equal, 1 with a unified diff otherwise.
"""

import argparse
import difflib
import json
import re
import sys

import yaml

HELM_LABELS = {
    "helm.sh/chart",
    "app.kubernetes.io/instance",
    "app.kubernetes.io/managed-by",
    "app.kubernetes.io/version",
    "app.kubernetes.io/part-of",
}


def load(path):
    with open(path, encoding="utf-8") as f:
        return [d for d in yaml.safe_load_all(f) if d]


def strip_metadata(meta):
    labels = meta.get("labels")
    if labels is not None:
        for k in HELM_LABELS:
            labels.pop(k, None)
        if not labels:
            del meta["labels"]
    annotations = meta.get("annotations")
    if annotations is not None:
        for k in [k for k in annotations if k.startswith("checksum/")]:
            del annotations[k]
        if not annotations:
            del meta["annotations"]


def normalize(doc):
    strip_metadata(doc.setdefault("metadata", {}))
    template = doc.get("spec", {}).get("template")
    if isinstance(template, dict):
        strip_metadata(template.setdefault("metadata", {}))
        if not template["metadata"]:
            del template["metadata"]
    return doc


def key(doc):
    m = doc.get("metadata", {})
    return (doc.get("kind", ""), m.get("namespace", ""), m.get("name", ""))


def canonical(docs):
    return {key(d): json.dumps(d, indent=2, sort_keys=True, ensure_ascii=False) for d in docs}


def main():
    p = argparse.ArgumentParser()
    p.add_argument("expected")
    p.add_argument("actual")
    p.add_argument("--namespace", default="")
    p.add_argument("--unhash", action="append", default=[])
    p.add_argument("--object-namespace", default="")
    a = p.parse_args()

    expected = [
        d
        for d in load(a.expected)
        if not (d.get("kind") == "Namespace" and d["metadata"]["name"] == a.namespace)
    ]
    text = json.dumps(expected)
    for name in a.unhash:
        text = re.sub(re.escape(name) + r"-[a-z0-9]{10}\b", name, text)
    expected = [normalize(d) for d in json.loads(text)]
    if a.object_namespace:
        for d in expected:
            d["metadata"].setdefault("namespace", a.object_namespace)
    actual = [normalize(d) for d in load(a.actual)]

    e, h = canonical(expected), canonical(actual)
    if len(e) != len(expected) or len(h) != len(actual):
        print("duplicate objects in a render", file=sys.stderr)
        return 1
    failed = False
    for k in sorted(set(e) | set(h)):
        if e.get(k) == h.get(k):
            continue
        failed = True
        print(f"--- {'/'.join(k)} differs (- kustomize, + helm)")
        sys.stdout.writelines(
            difflib.unified_diff(
                (e.get(k, "") + "\n").splitlines(True),
                (h.get(k, "") + "\n").splitlines(True),
                "kustomize",
                "helm",
            )
        )
    if not failed:
        print(f"{len(e)} objects equal")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
