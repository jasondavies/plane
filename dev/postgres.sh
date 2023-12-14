#!/bin/sh

set -e

docker run \
    -d \
    -e POSTGRES_HOST_AUTH_METHOD=trust \
    --name postgres \
    -p 5432:5432 \
    postgres:16

echo "Postgres is now running. Run 'docker stop postgres && docker rm postgres' to remove it."
