"use strict";

const {
  RelayClient,
  RelayerTransactionState,
  RelayerTxType,
} = require("@polymarket/builder-relayer-client");
const { Wallet, ethers } = require("ethers");

const ZERO_BYTES32 =
  "0x0000000000000000000000000000000000000000000000000000000000000000";
const CTF_ABI = [
  "function redeemPositions(address collateralToken, bytes32 parentCollectionId, bytes32 conditionId, uint256[] indexSets)",
];

function output(payload) {
  process.stdout.write(`${JSON.stringify(payload)}\n`);
}

function fail(message) {
  output({ ok: false, error: String(message || "unknown error") });
  process.exit(1);
}

function waitMs(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function getEnv(name, required = true) {
  const value = process.env[name];
  const trimmed = typeof value === "string" ? value.trim() : "";
  if (required && trimmed.length === 0) {
    throw new Error(`${name} is required`);
  }
  return trimmed;
}

function parsePositiveInt(name, fallback) {
  const raw = getEnv(name, false);
  if (raw.length === 0) {
    return fallback;
  }

  const parsed = Number.parseInt(raw, 10);
  if (!Number.isFinite(parsed) || parsed <= 0) {
    throw new Error(`${name} must be > 0, got: ${raw}`);
  }

  return parsed;
}

async function withTimeout(promise, timeoutMs, label) {
  let timeoutId = null;

  try {
    const result = await Promise.race([
      promise,
      new Promise((_, reject) => {
        timeoutId = setTimeout(() => {
          reject(new Error(`${label} timeout after ${timeoutMs}ms`));
        }, timeoutMs);
      }),
    ]);

    return result;
  } finally {
    if (timeoutId !== null) {
      clearTimeout(timeoutId);
    }
  }
}

async function getWorkingPolygonProvider(rpcCsv, timeoutMs) {
  const urls = rpcCsv
    .split(",")
    .map((value) => value.trim())
    .filter((value) => value.length > 0);

  if (urls.length === 0) {
    throw new Error("POLYGON_RPC_URL does not contain a usable endpoint");
  }

  let lastError = null;

  for (const rpc of urls) {
    try {
      const provider = new ethers.providers.StaticJsonRpcProvider(rpc, {
        name: "matic",
        chainId: 137,
      });

      const network = await withTimeout(
        provider.getNetwork(),
        timeoutMs,
        `provider.getNetwork(${rpc})`,
      );
      if (Number(network.chainId) !== 137) {
        throw new Error(`unexpected chainId ${network.chainId} on ${rpc}`);
      }

      return provider;
    } catch (error) {
      lastError = error;
    }
  }

  throw new Error(
    `all polygon rpc endpoints failed: ${lastError ? String(lastError.message || lastError) : "unknown"}`,
  );
}

function attachRelayerApiHeaders(client, apiKey, apiKeyAddress) {
  const mutable = client;
  if (mutable.__headersPatched === true) {
    return;
  }

  if (typeof mutable.send !== "function") {
    throw new Error("Relayer SDK internal send method not available");
  }

  const originalSend = mutable.send.bind(client);

  mutable.send = async (endpoint, method, options) => {
    const mergedHeaders = {
      ...(options && options.headers ? options.headers : {}),
      RELAYER_API_KEY: apiKey,
      RELAYER_API_KEY_ADDRESS: apiKeyAddress,
    };

    return originalSend(endpoint, method, {
      ...(options || {}),
      headers: mergedHeaders,
    });
  };

  mutable.__headersPatched = true;
}

async function pollRelayerSuccess(
  client,
  transactionId,
  pollIntervalMs,
  maxPolls,
  timeoutMs,
) {
  const successStates = new Set([
    String(RelayerTransactionState.STATE_MINED || "").toUpperCase(),
    String(RelayerTransactionState.STATE_CONFIRMED || "").toUpperCase(),
  ]);

  const failedStates = new Set([
    String(RelayerTransactionState.STATE_FAILED || "").toUpperCase(),
    String(RelayerTransactionState.STATE_INVALID || "").toUpperCase(),
  ]);

  for (let attempt = 1; attempt <= maxPolls; attempt++) {
    const txs = await withTimeout(
      client.getTransaction(transactionId),
      timeoutMs,
      "relayer getTransaction",
    );
    const tx = Array.isArray(txs) ? txs[0] : undefined;
    const state = String((tx && tx.state) || "").toUpperCase();

    if (successStates.has(state)) {
      return {
        tx,
        finalState: state,
        pollAttempts: attempt,
      };
    }

    if (failedStates.has(state)) {
      throw new Error(`Relayer tx failed with state ${state}`);
    }

    if (attempt < maxPolls) {
      await waitMs(pollIntervalMs);
    }
  }

  throw new Error(
    `RELAYER_TIMEOUT: ${transactionId} did not reach success state after ${maxPolls} polls`,
  );
}

async function main() {
  const conditionId = getEnv("CONDITION_ID");
  if (!/^0x[a-fA-F0-9]{64}$/.test(conditionId)) {
    throw new Error(`Invalid conditionId for relayer redeem: ${conditionId}`);
  }

  const relayerBaseUrl = getEnv("RELAYER_BASE_URL");
  const relayerApiKey = getEnv("RELAYER_API_KEY");
  const relayerApiKeyAddress = getEnv("RELAYER_API_KEY_ADDRESS");
  const privateKey = getEnv("PRIVATE_KEY");
  const rpcCsv = getEnv("POLYGON_RPC_URL");
  const ctfContract = getEnv("CTF_CONTRACT");
  const usdcAddress = getEnv("USDC_E");

  const requestTimeoutMs = parsePositiveInt(
    "RELAYER_REQUEST_TIMEOUT_MS",
    30000,
  );
  const pollIntervalMs = parsePositiveInt("RELAYER_POLL_INTERVAL_MS", 2000);
  const maxPolls = parsePositiveInt("RELAYER_MAX_POLLS", 120);

  const provider = await getWorkingPolygonProvider(rpcCsv, requestTimeoutMs);
  const signer = new Wallet(privateKey, provider);

  const client = new RelayClient(
    relayerBaseUrl,
    137,
    signer,
    undefined,
    RelayerTxType.SAFE,
  );

  attachRelayerApiHeaders(client, relayerApiKey, relayerApiKeyAddress);

  const iface = new ethers.utils.Interface(CTF_ABI);
  const calldata = iface.encodeFunctionData("redeemPositions", [
    usdcAddress,
    ZERO_BYTES32,
    conditionId,
    [1, 2],
  ]);

  const tx = {
    to: ctfContract,
    data: calldata,
    value: "0",
  };

  const submit = await withTimeout(
    client.execute([tx], `Redeem CTF positions for ${conditionId}`),
    requestTimeoutMs,
    "relayer execute",
  );

  const transactionId = String((submit && submit.transactionID) || "").trim();
  if (!transactionId) {
    throw new Error("Relayer execute returned empty transaction ID");
  }

  const terminal = await pollRelayerSuccess(
    client,
    transactionId,
    pollIntervalMs,
    maxPolls,
    requestTimeoutMs,
  );

  const txHash = String(
    (terminal.tx && terminal.tx.transactionHash) ||
      (submit && submit.transactionHash) ||
      (submit && submit.hash) ||
      "",
  ).trim();

  if (!/^0x[a-fA-F0-9]{64}$/.test(txHash)) {
    throw new Error(
      `Relayer success state reached but tx hash invalid/missing for ${transactionId}`,
    );
  }

  const receipt = await withTimeout(
    provider.waitForTransaction(txHash, 1),
    requestTimeoutMs,
    "provider.waitForTransaction",
  );

  if (!receipt || Number(receipt.status || 0) !== 1) {
    throw new Error(`Relayer tx not confirmed successfully: ${txHash}`);
  }

  output({
    ok: true,
    txHash,
    transactionId,
    finalState: terminal.finalState,
    pollAttempts: terminal.pollAttempts,
  });
}

main().catch((error) => {
  const message = error instanceof Error ? error.message : String(error);
  fail(message);
});
