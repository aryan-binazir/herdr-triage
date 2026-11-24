#!/bin/sh
# Versioning: every PR bumps the version (patch unless told otherwise);
# after it merges to main, `tag` publishes that version as a GitHub release.
#
#   scripts/release.sh bump [patch|minor|major]   in a PR branch
#   scripts/release.sh tag [--dry-run]            after merging to main
set -eu

cd "$(dirname "$0")/.."

die() {
	echo "release: $*" >&2
	exit 1
}

usage() {
	die "usage: $0 bump [patch|minor|major] | tag [--dry-run]"
}

last_tag() {
	git fetch --quiet --tags origin main
	git describe --tags --abbrev=0 origin/main
}

# Reads a manifest on stdin and prints its first `version = "X.Y.Z"`.
manifest_version() {
	sed -n 's/^version = "\([0-9]*\.[0-9]*\.[0-9]*\)"$/\1/p' | head -n 1
}

# Prints a positive, zero or negative number as $1 is above, equal to or below $2.
compare_versions() {
	echo "$1 $2" | awk '{
		split($1, a, "."); split($2, b, ".")
		for (i = 1; i <= 3; i++) if (a[i] != b[i]) { print a[i] - b[i]; exit }
		print 0
	}'
}

bump() {
	level=${1:-patch}
	last=$(last_tag)
	old=${last#v}
	major=${old%%.*}
	rest=${old#*.}
	minor=${rest%%.*}
	patch=${rest#*.}
	case $level in
	patch) new="$major.$minor.$((patch + 1))" ;;
	minor) new="$major.$((minor + 1)).0" ;;
	major) new="$((major + 1)).0.0" ;;
	*) usage ;;
	esac

	current=$(manifest_version <Cargo.toml)
	[ -n "$current" ] || die "no version found in Cargo.toml"

	# The first `version =` line is the package's own in both files.
	for file in Cargo.toml herdr-plugin.toml; do
		sed "1,/^version = /s/^version = \".*\"\$/version = \"$new\"/" "$file" >"$file.tmp"
		mv "$file.tmp" "$file"
	done
	cargo update --workspace --offline --quiet

	echo "release: $current -> $new ($level bump from $last)"
}

tag() {
	dry_run=
	case ${1:-} in
	'') ;;
	--dry-run) dry_run=1 ;;
	*) usage ;;
	esac

	last=$(last_tag)
	head=$(git rev-parse origin/main)
	if existing=$(git describe --tags --exact-match "$head" 2>/dev/null); then
		echo "release: origin/main ($head) is already tagged $existing"
		exit 0
	fi

	version=$(git show origin/main:Cargo.toml | manifest_version)
	[ -n "$version" ] || die "no version found in origin/main's Cargo.toml"
	plugin_version=$(git show origin/main:herdr-plugin.toml | manifest_version)
	[ "$plugin_version" = "$version" ] ||
		die "Cargo.toml ($version) and herdr-plugin.toml ($plugin_version) disagree on origin/main"
	[ "$(compare_versions "$version" "${last#v}")" -gt 0 ] ||
		die "origin/main's version $version is not above $last; run \`make release-bump\` in a PR"

	new="v$version"
	if [ -n "$dry_run" ]; then
		echo "release: would tag $new on origin/main ($head) and publish it with notes since $last"
		exit 0
	fi

	git tag -a "$new" -m "$new" "$head"
	git push origin "refs/tags/$new"
	gh release create "$new" --verify-tag --latest --title "$new" \
		--generate-notes --notes-start-tag "$last"
}

[ $# -ge 1 ] || usage
command=$1
shift
case $command in
bump)
	[ $# -le 1 ] || usage
	bump "$@"
	;;
tag)
	[ $# -le 1 ] || usage
	tag "$@"
	;;
*) usage ;;
esac
