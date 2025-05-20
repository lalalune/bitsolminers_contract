import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import { PublicKey } from "@solana/web3.js";
import { TOKEN_PROGRAM_ID } from "@solana/spl-token";
import { Bitsol } from "../target/types/bitsol";
import { setupTestProgram, setupTestPlayer } from "./test-helpers";

describe("Bitsol - global random reward", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace.Bitsol as Program<Bitsol>;

  const HALVING_INTERVAL = 10;
  const TOTAL_SUPPLY = new BN(1_000_000_000000);
  const INITIAL_REWARD_RATE = new BN(100_000000);
  const COOLDOWN_SLOTS = 1;

  let tokenMint: PublicKey,
      globalStateKey: PublicKey,
      playerWallet: anchor.web3.Keypair,
      playerKey: PublicKey,
      playerTokenAccount: PublicKey;

  beforeEach(async () => {
    ({ tokenMint, globalStateKey } = await setupTestProgram(
      provider,
      program,
      HALVING_INTERVAL,
      TOTAL_SUPPLY,
      INITIAL_REWARD_RATE,
      COOLDOWN_SLOTS,
    ));
    ({ playerWallet, playerKey, playerTokenAccount } = await setupTestPlayer(
      provider,
      program,
      tokenMint,
      globalStateKey,
    ));
  });

  it("allows claiming once and tracks supply", async () => {
    const rewardAmount = new BN(5_000000); // 5 BITS
    const expiry = new BN(5);

    await program.methods
      .generateGlobalRandomReward(rewardAmount, expiry)
      .accountsStrict({
        authority: provider.wallet.publicKey,
        globalState: globalStateKey,
        clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
      })
      .rpc();

    const beforeGs = await program.account.globalState.fetch(globalStateKey);

    await program.methods
      .claimGlobalRandomReward()
      .accountsStrict({
        playerWallet: playerWallet.publicKey,
        player: playerKey,
        globalState: globalStateKey,
        playerTokenAccount,
        tokenMint,
        tokenProgram: TOKEN_PROGRAM_ID,
        clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
      })
      .signers([playerWallet])
      .rpc();

    const afterGs = await program.account.globalState.fetch(globalStateKey);
    expect(afterGs.cumulativeRewards.sub(beforeGs.cumulativeRewards).toString()).toBe(rewardAmount.toString());

    await expect(
      program.methods
        .claimGlobalRandomReward()
        .accountsStrict({
          playerWallet: playerWallet.publicKey,
          player: playerKey,
          globalState: globalStateKey,
          playerTokenAccount,
          tokenMint,
          tokenProgram: TOKEN_PROGRAM_ID,
          clock: anchor.web3.SYSVAR_CLOCK_PUBKEY,
        })
        .signers([playerWallet])
        .rpc()
    ).rejects.toThrow(/NoPendingReward/);
  });
});
