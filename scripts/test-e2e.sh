#!/bin/bash -e

WORKDIR=${1:-$(pwd)}

setup_truffle() {
    echo "Installing truffle-hdwallet-provider."
    # Temporary fix for ethereumjs-wallet@0.6.1 incompatibility
    npm install ethereumjs-wallet@=0.6.0
    npm install truffle-hdwallet-provider
}

run_dummy_node_default() {
    echo "Starting dummy node."

    ekiden-node-dummy \
        --random-beacon-backend dummy \
        --entity-ethereum-address 0000000000000000000000000000000000000000 \
        --time-source-notifier mockrpc \
        --storage-backend dummy \
        &> dummy.log &
}

run_dummy_node_go_default() {
    local datadir=/tmp/ekiden-dummy-data
    rm -rf ${datadir}

    echo "Starting Go dummy node."

    ${WORKDIR}/ekiden-node \
        --log.level debug \
        --grpc.port 42261 \
        --epochtime.backend mock \
        --beacon.backend insecure \
        --storage.backend memory \
        --scheduler.backend trivial \
        --registry.backend memory \
        --datadir ${datadir} \
        &> dummy-go.log &
}

run_compute_node() {
    local id=$1
    shift
    local extra_args=$*

    # Generate port number.
    let "port=id + 10000"

    echo "Starting compute node ${id} on port ${port}."

    ekiden-compute \
        --no-persist-identity \
        --batch-storage multilayer \
        --storage-multilayer-sled-storage-base /tmp/ekiden-storage-persistent_${id} \
        --storage-multilayer-bottom-backend remote \
        --max-batch-timeout 100 \
        --time-source-notifier system \
        --entity-ethereum-address 0000000000000000000000000000000000000000 \
        --port ${port} \
        ${extra_args} \
        ${WORKDIR}/target/enclave/runtime-ethereum.so &> compute${id}.log &
}

run_test() {
    local dummy_node_runner=$1

    # Ensure cleanup on exit.
    trap 'kill -- -0' EXIT

    # Run the gateway. We start the gateway first so that we test 1) whether the
    # snapshot manager can recover after initially failing to connect to the
    # root hash stream, and 2) whether the gateway waits for the committee to be
    # elected and connects to the leader.
    echo "Starting web3 gateway."
    target/debug/gateway \
        --mr-enclave $(cat $WORKDIR/target/enclave/runtime-ethereum.mrenclave) \
        --threads 100 \
        --prometheus-metrics-addr 0.0.0.0:3001 \
        --prometheus-mode pull &> gateway.log &
    sleep 3

    # Start dummy node.
    $dummy_node_runner
    sleep 1

    # Start compute nodes.
    run_compute_node 1
    sleep 1
    run_compute_node 2

    # Advance epoch to elect a new committee.
    echo "Advancing epoch."
    sleep 2
    ekiden-node-dummy-controller set-epoch --epoch 1
    sleep 2

    # Run truffle tests
    echo "Running truffle tests."
    pushd ${WORKDIR}/tests/ > /dev/null
    truffle test
    popd > /dev/null

    # Dump the metrics.
    curl -v http://localhost:3001/metrics

    # Cleanup.
    echo "Cleaning up."
    pkill -P $$

    # Sleep to allow gateway's ports to be freed
    sleep 5
}

setup_truffle
run_test run_dummy_node_default
run_test run_dummy_node_go_default
