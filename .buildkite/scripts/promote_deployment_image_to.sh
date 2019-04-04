#! /bin/bash

#############################################
# Gets the deployment image tag from buildkite
# metadata and promotes the deployment image
# by retagging it with the provided tag.
#
# This script is intended to have buildkite
# specific things, like env vars and calling
# the buildkite-agent binary. Keeping this
# separate from the generic script that gets
# called allows us to use and test the generic
# scripts easily on a local dev box.
##############################################

# Helpful tips on writing build scripts:
# https://buildkite.com/docs/pipelines/writing-build-scripts
set -euxo pipefail

####################
# Required arguments
####################
new_image_tag=$1

#################
# Local variables
#################
docker_image_name=oasislabs/ekiden-runtime-ethereum
deployment_image_tag=$(buildkite-agent meta-data \
                       get \
                       "deployment_image_tag"
                     )
tag_suffix=${DEPLOYMENT_VARIANT:+-$DEPLOYMENT_VARIANT}

##############################################
# Add the provided tag to the deployment image
##############################################

echo 'test only. skipping promote step'
