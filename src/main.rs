use chrono;
use pdf::file::File;
use pdf::primitive::Primitive;
use serde::{Deserialize, Serialize};
use clap::{App, Arg};

enum ParserState {
    SearchingDividendEntry,
    SearchingINTCEntry,
    SearchingTaxEntry,
    SearchingGrossEntry,
}

struct Transaction {
    transaction_date: String,
    gross_us: f32,
    tax_us: f32,
    exchange_rate_date: String,
    exchange_rate: f32,
}

type ReqwestClient = reqwest::blocking::Client;

// Example response: {"table":"A",
//                    "currency":"dolar amerykański",
//                    "code":"USD",
//                    "rates":[{"no":"039/A/NBP/2021",
//                              "effectiveDate":"2021-02-26",
//                              "mid":3.7247}]}

#[derive(Debug, Deserialize, Serialize)]
struct NBPResponse<T> {
    table: String,
    currency: String,
    code: String,
    rates: Vec<T>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ExchangeRate {
    no: String,
    effectiveDate: String,
    mid: f32,
}

fn init_logging_infrastructure() {
    // TODO(jczaja): test on windows/macos
    syslog::init(
        syslog::Facility::LOG_USER,
        log::LevelFilter::Debug,
        Some("e-trade-tax-helper"),
    )
    .expect("Error initializing syslog");
}

fn get_exchange_rate(transaction_date: &str) -> Result<(String, f32), String> {
    // proxies are taken from env vars: http_proxy and https_proxy
    let http_proxy = std::env::var("http_proxy");
    let https_proxy = std::env::var("https_proxy");

    // If there is proxy then pick first URL
    let base_client = ReqwestClient::builder();
    let client = match &http_proxy {
        Ok(proxy) => {
            base_client.proxy(reqwest::Proxy::http(proxy).expect("Error setting HTTP proxy"))
        }
        Err(_) => base_client,
    };
    let client = match &https_proxy {
        Ok(proxy) => client.proxy(reqwest::Proxy::https(proxy).expect("Error setting HTTP proxy")),
        Err(_) => client,
    };
    let client = client.build().expect("Could not create REST API client");

    let base_exchange_rate_url = "http://api.nbp.pl/api/exchangerates/rates/a/usd/";
    let mut converted_date =
        chrono::NaiveDate::parse_from_str(transaction_date, "%m/%d/%y").unwrap();

    // Try to get exchange rate going backwards with dates till success
    let mut is_success = false;
    let mut exchange_rate = 0.0;
    let mut exchange_rate_date: String = "N/A".to_string();
    while is_success == false {
        converted_date = converted_date
            .checked_sub_signed(chrono::Duration::days(1))
            .expect("Error traversing date");

        let exchange_rate_url: String = base_exchange_rate_url.to_string()
            + &format!("{}", converted_date.format("%Y-%m-%d"))
            + "/?format=json";

        let body = client.get(&(exchange_rate_url)).send();
        let actual_body = body.expect(&format!(
            "Getting Exchange Rate from NBP ({}) failed",
            exchange_rate_url
        ));
        is_success = actual_body.status().is_success();
        if is_success == true {
            log::info!("RESPONSE {:#?}", actual_body);

            let nbp_response = actual_body
                .json::<NBPResponse<ExchangeRate>>()
                .expect("Error converting response to JSON");
            log::info!("body of exchange_rate = {:#?}", nbp_response);
            exchange_rate = nbp_response.rates[0].mid;
            exchange_rate_date = format!("{}", converted_date.format("%Y-%m-%d"));
        }
    }

    Ok((exchange_rate_date, exchange_rate))
}

fn parse_brokerage_statement(pdftoparse: &str) -> Result<(String, f32, f32), String> {
    //2. parsing each pdf
    let mypdffile = File::<Vec<u8>>::open(pdftoparse).unwrap();

    let mut state = ParserState::SearchingDividendEntry;
    let mut transaction_date: String = "N/A".to_string();
    let mut tax_us = 0.0;

    log::info!("Parsing: {}", pdftoparse);
    for page in mypdffile.pages() {
        let page = page.unwrap();
        let contents = page.contents.as_ref().unwrap();
        for op in contents.operations.iter() {
            match op.operator.as_ref() {
                "TJ" => {
                    // Text show
                    if op.operands.len() > 0 {
                        //transaction_date = op.operands[0];
                        let a = &op.operands[0];
                        match a {
                            Primitive::Array(c) => {
                                // If string is "Dividend"
                                if let Primitive::String(actual_string) = &c[0] {
                                    match state {
                                        ParserState::SearchingDividendEntry => {
                                            let rust_string =
                                                actual_string.clone().into_string().unwrap();
                                            if rust_string == "Dividend" {
                                                state = ParserState::SearchingINTCEntry;
                                            } else {
                                                transaction_date = rust_string;
                                            }
                                        }
                                        ParserState::SearchingINTCEntry => {
                                            let rust_string =
                                                actual_string.clone().into_string().unwrap();
                                            if rust_string == "INTC" {
                                                state = ParserState::SearchingTaxEntry;
                                            }
                                        }
                                        ParserState::SearchingTaxEntry => {
                                            tax_us = actual_string
                                                .clone()
                                                .into_string()
                                                .unwrap()
                                                .parse::<f32>()
                                                .unwrap();
                                            state = ParserState::SearchingGrossEntry
                                        }
                                        ParserState::SearchingGrossEntry => {
                                            let gross_us = actual_string
                                                .clone()
                                                .into_string()
                                                .unwrap()
                                                .parse::<f32>()
                                                .unwrap();
                                            state = ParserState::SearchingDividendEntry;
                                            return Ok((transaction_date, gross_us, tax_us));
                                        }
                                    }
                                }
                            }
                            _ => (),
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Err(format!("Error parsing pdf: {}", pdftoparse))
}

fn compute_tax(transactions: Vec<Transaction>) -> (f32, f32) {
    // Gross income from dividends in PLN
    let gross_us_pl: f32 = transactions
        .iter()
        .map(|x| x.exchange_rate * x.gross_us)
        .sum();
    // Tax paind in US in PLN
    let tax_us_pl: f32 = transactions
        .iter()
        .map(|x| x.exchange_rate * x.tax_us)
        .sum();
    (gross_us_pl, tax_us_pl)
}

fn main() {
    init_logging_infrastructure();

    let matches = App::new("E-trade tax helper")
    .arg(
        Arg::with_name("residence")
            .long("residence")
            .help("Country of residence e.g. Poland")
            .value_name("FILE")
            .takes_value(true)
            .default_value("pl"),
    )
    .arg(
        Arg::with_name("pdf documents")
            .help("Brokerage statement PDF files")
            .multiple(true)
    )
    .get_matches();


    let residence = matches.value_of("residence").expect("error getting residence value");
    let pdfnames =  matches.values_of("pdf documents").expect("error getting brokarage statements pdfs names");

    let mut transactions: Vec<Transaction> = Vec::new();
    let args: Vec<String> = std::env::args().collect();

    log::info!("Started e-trade-tax-helper");
    // Start from second one
    for pdfname in pdfnames {
        // 1. Get PDF parsed and attach exchange rate
        log::info!("Processing: {}", pdfname);
        let p = parse_brokerage_statement(&pdfname);

        if let Ok((transaction_date, gross_us, tax_us)) = p {
            let (exchange_rate_date, exchange_rate) =
                get_exchange_rate(&transaction_date).expect("Error getting exchange rate");
            let msg = format!(
                "TRANSACTION date: {}, gross: ${}, tax_us: ${}, exchange_rate: {} pln, exchange_rate_date: {}",
                &transaction_date, &gross_us, &tax_us, &exchange_rate, &exchange_rate_date
            )
            .to_owned();
            println!("{}", msg);
            log::info!("{}", msg);
            transactions.push(Transaction {
                transaction_date,
                gross_us,
                tax_us,
                exchange_rate_date,
                exchange_rate,
            });
        }
    }
    let (gross_us_pl, tax_us_pl) = compute_tax(transactions);
    println!("===> PRZYCHOD Z ZAGRANICY: {} PLN", gross_us_pl);
    println!("===> PODATEK ZAPLACONY ZAGRANICA: {} PLN", tax_us_pl);
    // Expected full TAX in Poland
    let full_tax_pl = gross_us_pl * 19.0 / 100.0;
    let tax_diff_to_pay_pl = full_tax_pl - tax_us_pl;
    println!("DOPLATA: {} PLN", tax_diff_to_pay_pl);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exchange_rate() -> Result<(), String> {
        assert_eq!(
            get_exchange_rate("03/01/21"),
            Ok(("2021-02-26".to_owned(), 3.7247))
        );
        Ok(())
    }

    #[test]
    #[ignore]
    fn test_parse_brokerage_statement() -> Result<(), String> {
        assert_eq!(
            parse_brokerage_statement("data/example.pdf"),
            Ok(("03/01/21".to_owned(), 574.42, 86.16))
        );
        assert_eq!(
            parse_brokerage_statement("data/example2.pdf"),
            Err(format!("Error parsing pdf: data/example2.pdf"))
        );

        Ok(())
    }

    #[test]
    fn test_simple_computation() -> Result<(), String> {
        // Init Transactions
        let transactions: Vec<Transaction> = vec![Transaction {
            transaction_date: "N/A".to_string(),
            gross_us: 100.0,
            tax_us: 25.0,
            exchange_rate_date: "N/A".to_string(),
            exchange_rate: 4.0,
        }];
        assert_eq!(compute_tax(transactions), (400.0, 100.0));
        Ok(())
    }

    #[test]
    fn test_computation() -> Result<(), String> {
        // Init Transactions
        let transactions: Vec<Transaction> = vec![
            Transaction {
                transaction_date: "N/A".to_string(),
                gross_us: 100.0,
                tax_us: 25.0,
                exchange_rate_date: "N/A".to_string(),
                exchange_rate: 4.0,
            },
            Transaction {
                transaction_date: "N/A".to_string(),
                gross_us: 126.0,
                tax_us: 10.0,
                exchange_rate_date: "N/A".to_string(),
                exchange_rate: 3.5,
            },
        ];
        assert_eq!(
            compute_tax(transactions),
            (400.0 + 126.0 * 3.5, 100.0 + 10.0 * 3.5)
        );
        Ok(())
    }
}

// TODO: cutting out personal info
