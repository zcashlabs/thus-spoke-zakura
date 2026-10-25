"""Validate Zakura Common identities in cargo metadata JSON read from stdin."""

import json
import sys


# Published, coordinated members of zakura-core/common at v4.0.0. The
# unpublished bento/udon helpers and wallet-library releases version separately.
COMMON_UPSTREAM = {
    "zakura-bellman": "bellman",
    "zakura-bls12-381": "bls12_381",
    "zakura-equihash": "equihash",
    "zakura-halo2-gadgets": "halo2_gadgets",
    "zakura-halo2-poseidon": "halo2_poseidon",
    "zakura-halo2-proofs": "halo2_proofs",
    "zakura-jubjub": "jubjub",
    "zakura-orchard": "orchard",
    "zakura-orchard-legacy": "orchard-legacy",
    "zakura-pairing": "pairing",
    "zakura-pasta-curves": "pasta_curves",
    "zakura-reddsa": "reddsa",
    "zakura-reddsa-frost": "reddsa-frost",
    "zakura-redjubjub": "redjubjub",
    "zakura-sapling-crypto": "sapling-crypto",
    "zakura-sinsemilla": "sinsemilla",
    "zakura-address": "zcash_address",
    "zakura-keys": "zcash_keys",
    "zakura-note-encryption": "zcash_note_encryption",
    "zakura-primitives": "zcash_primitives",
    "zakura-protocol": "zcash_protocol",
    "zakura-proofs": "zcash_proofs",
    "zakura-transparent": "zcash_transparent",
    "zakura-zip321": "zip321",
}
REQUIRED_DIRECT = {
    "zakura-keys", "zakura-primitives",
    "zakura-proofs", "zakura-orchard", "zakura-sapling-crypto",
    "zakura-transparent", "zakura-protocol", "zakura-zip321",
}
REQUIRED_PACKAGES = REQUIRED_DIRECT | {"zakura-note-encryption"}


def violations(metadata):
    packages = metadata["packages"]
    workspace = set(metadata["workspace_members"])
    versions = {}
    direct = set()
    errors = []
    for package in packages:
        name = package["name"]
        if name in COMMON_UPSTREAM.values():
            errors.append(f"upstream Common counterpart is forbidden: {name}")
        if name in COMMON_UPSTREAM:
            versions.setdefault(name, []).append(package["version"])
        if package["id"] in workspace:
            direct.update(dependency["name"] for dependency in package["dependencies"])
    for name, resolved in sorted(versions.items()):
        if len(resolved) != 1 or any(version.split(".", 1)[0] != "4" for version in resolved):
            errors.append(f"{name} must resolve once at 4.x; found {', '.join(sorted(resolved))}")
    for name in sorted(REQUIRED_PACKAGES):
        if name not in versions:
            errors.append(f"required Common package is missing: {name}")
    for name in sorted(REQUIRED_DIRECT):
        if name not in direct:
            errors.append(f"required direct Common dependency is missing: {name}")
    return errors


def main():
    try:
        errors = violations(json.load(sys.stdin))
    except (KeyError, TypeError, ValueError) as error:
        print(f"invalid cargo metadata: {error}", file=sys.stderr)
        return 1
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("Zakura Common dependency identities and versions are coherent")
    return 0


if __name__ == "__main__":
    sys.exit(main())
