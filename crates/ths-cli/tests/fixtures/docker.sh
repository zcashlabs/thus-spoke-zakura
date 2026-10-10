#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$THS_TEST_COMMANDS"
case "$1 $2" in
    'version --format')
        if [ "${THS_TEST_FAIL_DOCTOR:-0}" = 1 ]; then exit 1; fi
        printf 'test-docker\n'
        ;;
    'image inspect')
        if [ "${THS_TEST_MISSING_IMAGE:-0}" = 1 ]; then exit 1; fi
        ;;
    'inspect --format') printf '%s\n' "$THS_TEST_PORT" ;;
    'container ls'|'volume ls'|'network ls')
        for resource in "$THS_TEST_STATE/$1"/*; do
            [ -f "$resource" ] || continue
            basename "$resource"
        done
        ;;
    'container inspect'|'volume inspect'|'network inspect')
        kind=$1
        shift 2
        if [ "$1" = '--format' ]; then
            case "$2" in
                *HostPort*) printf '%s\n' "$THS_TEST_PORT" ;;
                *Running*) printf 'true\n' ;;
                *) exit 1 ;;
            esac
        else
            printf '[{"Id":"%s","Name":"%s","Labels":{"com.zakura.ths.instance":"%s"},"Config":{"Labels":{"com.zakura.ths.instance":"%s"}}}]\n' "$1" "$1" "$THS_TEST_NAME" "$THS_TEST_NAME"
        fi
        ;;
    'network create'|'volume create')
        kind=$1
        for target do :; done
        mkdir -p "$THS_TEST_STATE/$kind"
        touch "$THS_TEST_STATE/$kind/$target"
        printf '%s\n' "$target"
        ;;
    'create --name')
        mkdir -p "$THS_TEST_STATE/container"
        touch "$THS_TEST_STATE/container/$3"
        printf '%s\n' "$3"
        ;;
    'start -a')
        printf 'disposable test mnemonic\n'
        printf 'init diagnostic\n' >&2
        if [ "${THS_TEST_FAIL_INIT:-0}" = 1 ]; then exit 1; fi
        ;;
    'rm -f')
        shift 2
        for target do rm "$THS_TEST_STATE/container/$target"; done
        printf 'docker lifecycle output\n'
        ;;
    'volume rm'|'network rm')
        kind=$1
        shift 2
        for target do rm "$THS_TEST_STATE/$kind/$target"; done
        printf 'docker lifecycle output\n'
        ;;
    'start '*|'build '*|'pull '*) printf 'docker lifecycle output\n' ;;
    *) printf 'unexpected Docker command: %s\n' "$*" >&2; exit 1 ;;
esac
