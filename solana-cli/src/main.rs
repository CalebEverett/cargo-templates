use clap::{
    crate_description, crate_name, crate_version, value_t, value_t_or_exit, App, AppSettings, Arg,
    ArgMatches, SubCommand,
};
use solana_clap_utils::{
    fee_payer::fee_payer_arg,
    input_parsers::pubkey_of_signer,
    input_validators::{
        is_parsable, is_url_or_moniker, is_valid_pubkey, is_valid_signer,
        normalize_to_url_if_moniker,
    },
    keypair::{signer_from_path, CliSignerInfo},
    memo::memo_arg,
};
use solana_cli_output::{CliSignature, OutputFormat};
use solana_client::rpc_client::RpcClient;
use solana_remote_wallet::remote_wallet::RemoteWalletManager;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::Instruction,
    message::Message,
    native_token::lamports_to_sol,
    program_pack::Pack,
    pubkey::Pubkey,
    signer::{keypair::Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};
use spl_token::{self, instruction, native_mint, state};
use std::process::exit;
use std::sync::Arc;

pub mod config;
use config::Config;

pub mod output;
use output::{println_display, CliTokenAmount};

pub(crate) type Error = Box<dyn std::error::Error>;
type CommandResult = Result<Option<(u64, Vec<Vec<Instruction>>)>, Error>;

fn is_mint_decimals(string: String) -> Result<(), String> {
    is_parsable::<u8>(string)
}

fn new_throwaway_signer() -> (Box<dyn Signer>, Pubkey) {
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    (Box::new(keypair) as Box<dyn Signer>, pubkey)
}

fn get_signer(
    matches: &ArgMatches<'_>,
    keypair_name: &str,
    wallet_manager: &mut Option<Arc<RemoteWalletManager>>,
) -> Option<(Box<dyn Signer>, Pubkey)> {
    matches.value_of(keypair_name).map(|path| {
        let signer =
            signer_from_path(matches, path, keypair_name, wallet_manager).unwrap_or_else(|e| {
                eprintln!("error: {}", e);
                exit(1);
            });
        let signer_pubkey = signer.pubkey();
        (signer, signer_pubkey)
    })
}

pub(crate) fn check_fee_payer_balance(config: &Config, required_balance: u64) -> Result<(), Error> {
    let balance = config.rpc_client.get_balance(&config.fee_payer)?;
    if balance < required_balance {
        Err(format!(
            "Fee payer, {}, has insufficient balance: {} required, {} available",
            config.fee_payer,
            lamports_to_sol(required_balance),
            lamports_to_sol(balance)
        )
        .into())
    } else {
        Ok(())
    }
}

fn main() {
    let default_decimals = &format!("{}", native_mint::DECIMALS);
    let no_wait = false;
    let app_matches = App::new(crate_name!())
        .about(crate_description!())
        .version(crate_version!())
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .arg({
            let arg = Arg::with_name("config_file")
                .short("C")
                .long("config")
                .value_name("PATH")
                .takes_value(true)
                .global(true)
                .help("Configuration file to use");
            if let Some(ref config_file) = *solana_cli_config::CONFIG_FILE {
                arg.default_value(config_file)
            } else {
                arg
            }
        })
        .arg(
            Arg::with_name("json_rpc_url")
                .short("u")
                .long("url")
                .value_name("URL_OR_MONIKER")
                .takes_value(true)
                .global(true)
                .validator(is_url_or_moniker)
                .help(
                    "URL for Solana's JSON RPC or moniker (or their first letter): \
                       [mainnet-beta, testnet, devnet, localhost] \
                    Default from the configuration file.",
                ),
        )
        .arg(
            Arg::with_name("verbose")
                .short("v")
                .long("verbose")
                .takes_value(false)
                .global(true)
                .help("Show additional information"),
        )
        .arg(
            Arg::with_name("output_format")
                .long("output")
                .value_name("FORMAT")
                .global(true)
                .takes_value(true)
                .possible_values(&["json", "json-compact"])
                .help("Return information in specified output format"),
        )
        .arg(fee_payer_arg().global(true))
        .subcommand(
            SubCommand::with_name("create-token")
                .about("Create a new token")
                .arg(
                    Arg::with_name("token_keypair")
                        .value_name("TOKEN_KEYPAIR")
                        .validator(is_valid_signer)
                        .takes_value(true)
                        .index(1)
                        .help(
                            "Specify the token keypair. \
                             This may be a keypair file or the ASK keyword. \
                             [default: randomly generated keypair]",
                        ),
                )
                .arg(
                    Arg::with_name("mint_authority")
                        .long("mint-authority")
                        .alias("owner")
                        .value_name("ADDRESS")
                        .validator(is_valid_pubkey)
                        .takes_value(true)
                        .help(
                            "Specify the mint authority address. \
                             Defaults to the client keypair address.",
                        ),
                )
                .arg(
                    Arg::with_name("decimals")
                        .long("decimals")
                        .validator(is_mint_decimals)
                        .value_name("DECIMALS")
                        .takes_value(true)
                        .default_value(default_decimals)
                        .help("Number of base 10 digits to the right of the decimal place"),
                )
                .arg(
                    Arg::with_name("enable_freeze")
                        .long("enable-freeze")
                        .takes_value(false)
                        .help("Enable the mint authority to freeze associated token accounts."),
                )
                .arg(memo_arg()),
        )
        .subcommand(
            SubCommand::with_name("supply")
                .about("Get token supply")
                .arg(
                    Arg::with_name("address")
                        .validator(is_valid_pubkey)
                        .value_name("TOKEN_ADDRESS")
                        .takes_value(true)
                        .index(1)
                        .required(true)
                        .help("The token address"),
                ),
        )
        .get_matches();

    let mut wallet_manager = None;
    let mut bulk_signers: Vec<Box<dyn Signer>> = Vec::new();

    let (sub_command, sub_matches) = app_matches.subcommand();
    let matches = sub_matches.unwrap();

    let config = {
        let cli_config = if let Some(config_file) = matches.value_of("config_file") {
            solana_cli_config::Config::load(config_file).unwrap_or_default()
        } else {
            solana_cli_config::Config::default()
        };
        let json_rpc_url = normalize_to_url_if_moniker(
            matches
                .value_of("json_rpc_url")
                .unwrap_or(&cli_config.json_rpc_url),
        );

        let (signer, fee_payer) = signer_from_path(
            matches,
            matches
                .value_of("fee_payer")
                .unwrap_or(&cli_config.keypair_path),
            "fee_payer",
            &mut wallet_manager,
        )
        .map(|s| {
            let p = s.pubkey();
            (s, p)
        })
        .unwrap_or_else(|e| {
            eprintln!("error: {}", e);
            exit(1);
        });
        bulk_signers.push(signer);

        let verbose = matches.is_present("verbose");
        let output_format = matches
            .value_of("output_format")
            .map(|value| match value {
                "json" => OutputFormat::Json,
                "json-compact" => OutputFormat::JsonCompact,
                _ => unreachable!(),
            })
            .unwrap_or(if verbose {
                OutputFormat::DisplayVerbose
            } else {
                OutputFormat::Display
            });

        let dry_run = matches.is_present("dry_run");

        Config {
            rpc_client: RpcClient::new_with_commitment(json_rpc_url, CommitmentConfig::confirmed()),
            output_format,
            fee_payer,
            default_keypair_path: cli_config.keypair_path,
            dry_run,
        }
    };

    solana_logger::setup_with_default("solana=info");

    let _ = match (sub_command, sub_matches) {
        ("supply", Some(arg_matches)) => {
            let address = pubkey_of_signer(arg_matches, "address", &mut wallet_manager)
                .unwrap()
                .unwrap();
            command_supply(&config, address)
        }
        ("create-token", Some(arg_matches)) => {
            let decimals = value_t_or_exit!(arg_matches, "decimals", u8);
            let mint_authority =
                config.pubkey_or_default(arg_matches, "mint_authority", &mut wallet_manager);
            let memo = value_t!(arg_matches, "memo", String).ok();

            let (token_signer, token) =
                get_signer(arg_matches, "token_keypair", &mut wallet_manager)
                    .unwrap_or_else(new_throwaway_signer);
            bulk_signers.push(token_signer);

            command_create_token(
                &config,
                decimals,
                token,
                mint_authority,
                arg_matches.is_present("enable_freeze"),
                memo,
            )
        }
        _ => unreachable!(),
    }
    .and_then(|transaction_info| {
        if let Some((minimum_balance_for_rent_exemption, instruction_batches)) = transaction_info {
            let fee_payer = Some(&config.fee_payer);
            let signer_info = CliSignerInfo {
                signers: bulk_signers,
            };
            let (recent_blockhash, fee_calculator) = config.rpc_client.get_recent_blockhash()?;
            for instructions in instruction_batches {
                let message = Message::new(&instructions, fee_payer);
                check_fee_payer_balance(
                    &config,
                    minimum_balance_for_rent_exemption + fee_calculator.calculate_fee(&message),
                )?;
                let signers = signer_info.signers_for_message(&message);
                let mut transaction = Transaction::new_unsigned(message);
                transaction.try_sign(&signers, recent_blockhash)?;
                let signature = if no_wait {
                    config.rpc_client.send_transaction(&transaction)?
                } else {
                    config
                        .rpc_client
                        .send_and_confirm_transaction_with_spinner(&transaction)?
                };
                let signature = CliSignature {
                    signature: signature.to_string(),
                };
                println!("{}", config.output_format.formatted_string(&signature));
            }
        }
        Ok(())
    })
    .map_err(|err| {
        eprintln!("{}", err);
        exit(1);
    });
}

fn command_create_token(
    config: &Config,
    decimals: u8,
    token: Pubkey,
    authority: Pubkey,
    enable_freeze: bool,
    memo: Option<String>,
) -> CommandResult {
    println_display(config, format!("Creating token {}", token));

    let minimum_balance_for_rent_exemption = config
        .rpc_client
        .get_minimum_balance_for_rent_exemption(state::Mint::LEN)?;

    let freeze_authority_pubkey = if enable_freeze { Some(authority) } else { None };

    let mut instructions = vec![
        system_instruction::create_account(
            &config.fee_payer,
            &token,
            minimum_balance_for_rent_exemption,
            state::Mint::LEN as u64,
            &spl_token::id(),
        ),
        instruction::initialize_mint(
            &spl_token::id(),
            &token,
            &authority,
            freeze_authority_pubkey.as_ref(),
            decimals,
        )?,
    ];
    if let Some(text) = memo {
        instructions.push(spl_memo::build_memo(text.as_bytes(), &[&config.fee_payer]));
    }
    Ok(Some((
        minimum_balance_for_rent_exemption,
        vec![instructions],
    )))
}

fn command_supply(config: &Config, address: Pubkey) -> CommandResult {
    let supply = config.rpc_client.get_token_supply(&address)?;
    let cli_token_amount = CliTokenAmount { amount: supply };
    println!(
        "{}",
        config.output_format.formatted_string(&cli_token_amount)
    );

    Ok(None)
}
