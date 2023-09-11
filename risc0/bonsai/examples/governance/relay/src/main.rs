// Copyright 2023 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io::Write;

use anyhow::Context;
use bonsai_ethereum_relay::{EthersClientConfig, Relayer};
use bonsai_ethereum_relay_cli::{resolve_guest_entry, resolve_image_output, Output};
use bonsai_sdk::{
    alpha::{responses::SnarkProof, SdkErr},
    alpha_async::{get_client_from_parts, put_image},
};
use clap::{Args, Parser, Subcommand};
use ethers::{
    abi::{Hash, Token, Tokenizable},
    types::{Address, U256},
};
use methods::GUEST_LIST;
use risc0_zkvm::sha::Digest;

/// Index 0 private key generated by default in Anvil.
const ANVIL_DEFAULT_KEY: &'static str =
    "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

/// Interval between connection attempts in case of connection loss.
const CONNECTION_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Number of connection attempts.
const CONNECTION_RETRY_ATTEMPTS: u64 = 60;

#[derive(Subcommand)]
enum Command {
    /// Runs the RISC-V ELF binary.
    Query {
        /// The name of the guest binary
        guest_binary: String,

        /// The input to provide to the guest binary
        input: Option<String>,
    },
    /// Upload the RISC-V ELF binary to Bonsai.
    Upload {
        /// The name of the guest binary
        /// If not provided, all defined guests will be uploaded.
        guest_binary: Option<String>,
    },
    /// Upload the RISC-V ELF binary to Bonsai.
    Run {
        /// Bonsai Relay contract address on Ethereum
        #[arg(long, env)]
        relay_address: Address,

        /// Ethereum Node endpoint.
        #[arg(long, env, default_value = "ws://localhost:8545")]
        eth_node: String,

        /// Ethereum chain ID
        #[arg(long, default_value_t = 31337)]
        eth_chain_id: u64,

        /// Wallet Key Identifier.
        /// Can be a private key as a hex string, or an AWS KMS key identifier.
        /// Defaults to the first private key of a deafult Anvil instance.
        #[arg(
            short,
            long,
            env,
            default_value = ANVIL_DEFAULT_KEY
        )]
        private_key: String,

        /// Number of connection attempts.
        #[arg(short = 'a', long, env, default_value_t = CONNECTION_RETRY_ATTEMPTS)]
        connection_retry_attempts: u64,

        /// Interval between connection attempts.
        #[arg(short = 'i', long, env, default_value_t = CONNECTION_RETRY_INTERVAL.into(), value_parser = humantime::parse_duration)]
        connection_retry_interval: humantime::Duration,
    },
}

#[derive(Debug, Args)]
struct GlobalOpts {
    /// Bonsai API URL
    #[arg(long, env, global = true, default_value = "http://localhost:8081")]
    bonsai_api_url: String,

    /// Bonsai API key
    /// Defaults to empty, providing no authentication.
    #[arg(long, env, global = true, default_value = "")]
    bonsai_api_key: String,

    /// Toggle to enable dev_mode: only a local executor runs your
    /// zkVM program and no proof is generated.
    #[arg(long, env, global = true, default_value_t = false)]
    risc0_dev_mode: bool,
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct App {
    #[clap(flatten)]
    global_opts: GlobalOpts,

    #[command(subcommand)]
    command: Command,
}

/// Parse a slice of strings as a fixed array of uint256 tokens.
fn parse_to_tokens(slice: &[String]) -> anyhow::Result<Token> {
    Ok(Token::FixedArray(
        slice
            .iter()
            .map(|s| -> anyhow::Result<_> { Ok(U256::from_str_radix(s, 16)?.into_token()) })
            .collect::<Result<Vec<_>, _>>()?,
    ))
}

fn tokenize_snark_proof(proof: &SnarkProof) -> anyhow::Result<Token> {
    if proof.b.len() != 2 {
        anyhow::bail!("hex-strings encoded proof is not well formed");
    }
    for pair in [&proof.a, &proof.c].into_iter().chain(proof.b.iter()) {
        if pair.len() != 2 {
            anyhow::bail!("hex-strings encoded proof is not well formed");
        }
    }
    Ok(Token::FixedArray(vec![
        parse_to_tokens(&proof.a)?,
        Token::FixedArray(vec![
            parse_to_tokens(&proof.b[0])?,
            parse_to_tokens(&proof.b[1])?,
        ]),
        parse_to_tokens(&proof.c)?,
    ]))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = App::parse();
    let dev_mode = args.global_opts.risc0_dev_mode;

    match args.command {
        Command::Query {
            guest_binary,
            input,
        } => {
            // Search list for requested binary name
            let guest_entry = resolve_guest_entry(GUEST_LIST, &guest_binary)
                .context("failed to resolve guest entry")?;

            // Execute or return image id
            let output_tokens = match &input {
                // Input provided. Return the Ethereum ABI encoded journal and
                Some(input) => {
                    let output = resolve_image_output(input, &guest_entry, dev_mode)
                        .await
                        .context("failed to resolve image output")?;
                    match (dev_mode, output) {
                        (true, Output::Execution { journal }) => {
                            vec![Token::Bytes(journal)]
                        }
                        (
                            false,
                            Output::Bonsai {
                                journal,
                                receipt_metadata,
                                snark_proof,
                            },
                        ) => {
                            vec![
                                Token::Bytes(journal),
                                Hash::from(<[u8; 32]>::from(receipt_metadata.post.digest()))
                                    .into_token(),
                                Token::Bytes(ethers::abi::encode(&[tokenize_snark_proof(
                                    &snark_proof,
                                )?])),
                            ]
                        }
                        _ => {
                            anyhow::bail!("invalid dev mode and output combination: {:?}", dev_mode)
                        }
                    }
                }
                // No input. Return the Ethereum ABI encoded bytes32 image ID.
                None => vec![
                    Hash::from(bytemuck::cast::<_, [u8; 32]>(guest_entry.image_id)).into_token(),
                ],
            };

            let output = hex::encode(ethers::abi::encode(&output_tokens));
            print!("{output}");
            std::io::stdout()
                .flush()
                .context("failed to flush stdout buffer")?;
        }
        Command::Upload { guest_binary } => {
            let image_ids = upload_images(
                guest_binary,
                &args.global_opts.bonsai_api_url,
                &args.global_opts.bonsai_api_key,
            )
            .await?;

            let output = hex::encode(ethers::abi::encode(&[Token::Array(
                image_ids
                    .into_iter()
                    .map(|image_id| {
                        Hash::from(bytemuck::cast::<_, [u8; 32]>(image_id)).into_token()
                    })
                    .collect(),
            )]));
            print!("{output}");
            std::io::stdout()
                .flush()
                .context("failed to flush stdout buffer")?;
        }
        Command::Run {
            relay_address,
            eth_node,
            eth_chain_id,
            private_key,
            connection_retry_attempts,
            connection_retry_interval,
        } => {
            let relayer = Relayer {
                rest_api: true,
                dev_mode: dev_mode,
                rest_api_port: "8080".to_string(),
                bonsai_api_url: args.global_opts.bonsai_api_url.clone(),
                bonsai_api_key: args.global_opts.bonsai_api_key.clone(),
                relay_contract_address: relay_address,
            };
            let client_config = EthersClientConfig::new(
                eth_node,
                eth_chain_id,
                private_key.try_into()?,
                connection_retry_attempts,
                connection_retry_interval.into(),
            );
            let server_handle = tokio::spawn(relayer.run(client_config));

            // HACK: Wait 1 second to give local Bonsai a chance to start.
            std::thread::sleep(std::time::Duration::from_secs(1));

            // Upload all locally defined images.
            upload_images(
                None,
                &args.global_opts.bonsai_api_url,
                &args.global_opts.bonsai_api_key,
            )
            .await?;

            // Wait for the server to exit.
            let _ = server_handle.await;
        }
    }
    Ok(())
}

/// Upload a single specified image, or, if guest_binary is None, upload all
/// images in the GUEST_LIST. Returns a list of uploaded image IDs.
async fn upload_images(
    guest_binary: Option<String>,
    bonsai_api_url: &str,
    bonsai_api_key: &str,
) -> anyhow::Result<Vec<Digest>> {
    // Create a list of either the single binary name to upload or all guests.
    let guest_entries = guest_binary.map_or_else(
        || Ok::<_, anyhow::Error>(GUEST_LIST.iter().cloned().collect::<Vec<_>>()),
        |name| Ok(vec![resolve_guest_entry(&GUEST_LIST, &name)?]),
    )?;

    // Upload each guest binary.
    let mut image_ids = Vec::<Digest>::new();
    for guest_entry in guest_entries.iter() {
        // Search list for requested binary name
        let image_id = hex::encode(Vec::from(bytemuck::cast::<[u32; 8], [u8; 32]>(
            guest_entry.image_id,
        )));

        // upload binary to Bonsai
        let bonsai_client =
            get_client_from_parts(bonsai_api_url.to_string(), bonsai_api_key.to_string()).await?;
        let img_id = image_id.clone();

        match put_image(
            bonsai_client.clone(),
            img_id.clone(),
            guest_entry.elf.to_vec(),
        )
        .await
        {
            Ok(()) | Err(SdkErr::ImageIdExists) => Ok::<_, anyhow::Error>(()),
            Err(err) => Err(err.into()),
        }?;

        image_ids.push(guest_entry.image_id.into());
    }

    Ok(image_ids)
}
