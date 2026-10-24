#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$THS_TEST_COMMANDS"
case "$1 $2" in
    'version --format')
        if [ "${THS_TEST_FAIL_DOCTOR:-0}" != 0 ]; then
            printf 'Docker test failure\n' >&2
            exit 1
        fi
        printf 'test-docker\n'
        ;;
    'image inspect')
        [ "${THS_TEST_MISSING_IMAGE:-0}" = 0 ] || exit 1
        ;;
    'image ls') : ;;
    'build '*|'pull '*) printf 'docker lifecycle output\n' ;;
    'inspect --format')
        if [ "${THS_TEST_MISSING_INSPECT:-0}" != 0 ]; then
            for target do :; done
            printf 'error: no such object: %s\n' "$target" >&2
            exit 1
        fi
        printf '%s\n' "$THS_TEST_PORT"
        ;;
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
                *Created*)
                    state=running
                    case "$3" in *-init) state=exited ;; esac
                    if [ -n "${THS_TEST_EXITED_SERVICE:-}" ] &&
                        [ "$3" = "ths-$THS_TEST_NAME-$THS_TEST_EXITED_SERVICE" ]; then
                        state=exited
                    fi
                    printf '%s 2026-10-12T00:00:00Z 2026-10-12T00:01:00Z\n' "$state"
                    ;;
                *) exit 1 ;;
            esac
        else
            printf '[{"Id":"%s","Name":"%s","Labels":{"com.zakura.ths.instance":"%s"},"Config":{"Labels":{"com.zakura.ths.instance":"%s"}}}]\n' "$1" "$1" "$THS_TEST_NAME" "$THS_TEST_NAME"
        fi
        ;;
    'network create'|'volume create')
        kind=$1
        for target do :; done
        if [ "${THS_TEST_SLOW_CREATE:-}" = "$kind" ]; then
            sleep "${THS_TEST_SLOW_CREATE_SECONDS:-2}"
        fi
        mkdir -p "$THS_TEST_STATE/$kind"
        touch "$THS_TEST_STATE/$kind/$target"
        if [ "${THS_TEST_SLOW_CREATE:-}" = "$kind" ]; then
            printf 'finished %s create\n' "$kind" >> "$THS_TEST_COMMANDS"
        fi
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
    'rm '*)
        shift
        for target do rm -f "$THS_TEST_STATE/container/$target"; done
        printf 'docker lifecycle output\n'
        ;;
    'volume rm'|'network rm')
        kind=$1
        shift 2
        for target do rm -f "$THS_TEST_STATE/$kind/$target"; done
        printf 'docker lifecycle output\n'
        ;;
    'start '*) printf 'docker lifecycle output\n' ;;
    *) printf 'unexpected Docker command: %s\n' "$*" >&2; exit 1 ;;
esac
