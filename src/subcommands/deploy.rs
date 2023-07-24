use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use clap::Parser;
use colored::Colorize;
use starknet::{
    accounts::SingleOwnerAccount,
    contract::ContractFactory,
    core::{
        types::{BlockId, BlockTag, FieldElement},
        utils::{get_udc_deployed_address, UdcUniqueSettings, UdcUniqueness},
    },
    providers::Provider,
    signers::SigningKey,
};

use crate::{
    account::{AccountConfig, DeploymentStatus},
    address_book::AddressBookResolver,
    decode::FeltDecoder,
    fee::{FeeArgs, FeeSetting},
    path::ExpandedPathbufParser,
    signer::SignerArgs,
    utils::watch_tx,
    verbosity::VerbosityArgs,
    ProviderArgs,
};

/// The default UDC address: 0x041a78e741e5af2fec34b695679bc6891742439f7afb8484ecd7766661ad02bf.
const DEFAULT_UDC_ADDRESS: FieldElement = FieldElement::from_mont([
    15144800532519055890,
    15685625669053253235,
    9333317513348225193,
    121672436446604875,
]);

#[derive(Debug, Parser)]
pub struct Deploy {
    #[clap(flatten)]
    provider: ProviderArgs,
    #[clap(flatten)]
    signer: SignerArgs,
    #[clap(long, help = "Do not derive contract address from deployer address")]
    not_unique: bool,
    #[clap(
        long,
        env = "STARKNET_ACCOUNT",
        value_parser = ExpandedPathbufParser,
        help = "Path to account config JSON file"
    )]
    account: PathBuf,
    #[clap(flatten)]
    fee: FeeArgs,
    #[clap(long, help = "Use the given salt to compute contract deploy address")]
    salt: Option<String>,
    #[clap(long, help = "Wait for the transaction to confirm")]
    watch: bool,
    #[clap(help = "Class hash")]
    class_hash: String,
    #[clap(help = "Raw constructor arguments")]
    ctor_args: Vec<String>,
    #[clap(flatten)]
    verbosity: VerbosityArgs,
}

fn left_pad_with_zeros(input_string: &str, n: usize) -> String {
    if input_string.len() >= n {
        input_string.to_string()
    } else {
        let zeros_to_pad = n - input_string.len();
        let padded_string = format!("{}{}", "0".repeat(zeros_to_pad), input_string);
        padded_string
    }
}

impl Deploy {
    pub async fn run(self) -> Result<()> {
        self.verbosity.setup_logging();

        let fee_setting = self.fee.into_setting()?;

        let provider = Arc::new(self.provider.into_provider());
        let felt_decoder = FeltDecoder::new(AddressBookResolver::new(provider.clone()));

        if !self.account.exists() {
            anyhow::bail!("account config file not found");
        }

        let class_hash = FieldElement::from_hex_be(&self.class_hash)?;
        let mut ctor_args = vec![];
        for element in self.ctor_args.iter() {
            ctor_args.append(&mut felt_decoder.decode(element).await?);
        }

        let mut salt = 0;
        // TODO: refactor account & signer loading

        let account_config: AccountConfig =
            serde_json::from_reader(&mut std::fs::File::open(&self.account)?)?;

        let account_address = match account_config.deployment {
            DeploymentStatus::Undeployed(_) => anyhow::bail!("account not deployed"),
            DeploymentStatus::Deployed(inner) => inner.address,
        };

        
        let mut deployed_address: FieldElement;
        loop {
                deployed_address = get_udc_deployed_address(
                    FieldElement::from_dec_str(salt.to_string().as_str()).unwrap(),
                    class_hash,
                    &if self.not_unique {
                        UdcUniqueness::NotUnique
                    } else {
                        UdcUniqueness::Unique(UdcUniqueSettings {
                            deployer_address: account_address,
                            udc_contract_address: DEFAULT_UDC_ADDRESS,
                        })
                    },
                    &ctor_args,
                );
                
                let mut formated = format!("{:x}", deployed_address);
                formated = left_pad_with_zeros(&formated, 64);
                if formated.as_str().starts_with("04515") {
                    println!("Right salt is: {:?}", salt);
                    println!("Associated address: {:?}", formated);
                    break;
                }
                salt += 1;
        }
        let salt = FieldElement::from_dec_str(salt.to_string().as_str()).unwrap();
        let chain_id = provider.chain_id().await?;

        let signer = Arc::new(self.signer.into_signer()?);
        let mut account =
            SingleOwnerAccount::new(provider.clone(), signer.clone(), account_address, chain_id);
        account.set_block_id(BlockId::Tag(BlockTag::Pending));

        // TODO: allow custom UDC
        let factory = ContractFactory::new_with_udc(class_hash, account, DEFAULT_UDC_ADDRESS);

        // TODO: pre-compute and show target deployment address

        let contract_deployment = factory.deploy(&ctor_args, salt, !self.not_unique);

        let max_fee = match fee_setting {
            FeeSetting::Manual(fee) => fee,
            FeeSetting::EstimateOnly | FeeSetting::None => {
                let estimated_fee = contract_deployment.estimate_fee().await?.overall_fee;

                if fee_setting.is_estimate_only() {
                    eprintln!(
                        "{} ETH",
                        format!(
                            "{}",
                            <u64 as Into<FieldElement>>::into(estimated_fee).to_big_decimal(18)
                        )
                        .bright_yellow(),
                    );
                    return Ok(());
                }

                // TODO: make buffer configurable
                let estimated_fee_with_buffer = estimated_fee * 3 / 2;

                estimated_fee_with_buffer.into()
            }
        };

        eprintln!(
            "Deploying class {} with salt {}...",
            format!("{:#064x}", class_hash).bright_yellow(),
            format!("{:#064x}", salt).bright_yellow()
        );
        eprintln!(
            "The contract will be deployed at address {}",
            format!("{:#064x}", deployed_address).bright_yellow()
        );

        let deployment_tx = contract_deployment
            .max_fee(max_fee)
            .send()
            .await?
            .transaction_hash;
        eprintln!(
            "Contract deployment transaction: {}",
            format!("{:#064x}", deployment_tx).bright_yellow()
        );

        if self.watch {
            eprintln!(
                "Waiting for transaction {} to confirm...",
                format!("{:#064x}", deployment_tx).bright_yellow(),
            );
            watch_tx(&provider, deployment_tx).await?;
        }

        eprintln!("Contract deployed:");

        // Only the contract goes to stdout so this can be easily scripted
        println!("{}", format!("{:#064x}", deployed_address).bright_yellow());

        Ok(())
    }
}
