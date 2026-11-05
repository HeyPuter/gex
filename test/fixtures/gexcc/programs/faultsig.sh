#!/bin/sh
# faultsig.c: the verdicts only (gex says on stderr which signal each child died of; the host says nothing)
exec "$1" 2>/dev/null
