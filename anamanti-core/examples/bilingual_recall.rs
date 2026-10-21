//! Bilingual recall evaluation for the **current** local memory embedder
//! (`plans/BilingualPlan.md` §2.8 / Phase 3): nomic-embed-text-v1.5, unchanged by the
//! owner's decision. Facts are stored as spoken, so a household's store mixes English
//! and Portuguese; this measures how well each query language finds each fact language.
//!
//! It embeds a fixed mixed store (24 facts: 12 English, 12 the same facts in
//! Portuguese — as distractors for each other) and, for each query, ranks the store by
//! cosine similarity exactly as the GraphRAG backend does (asymmetric
//! `search_query:` / `search_document:` prefixes). Reported per direction:
//! recall@1 and recall@3 of the intended fact, restricted to facts in the target language
//! (so en→pt asks "is the Portuguese fact found when asked in English?").
//!
//!   cargo run --release --example bilingual_recall -- \
//!     "/path/to/models/nomic-embed-text-v1.5.onnx" "/path/to/models/nomic-tokenizer"
//!
//! Offline: no network, no keys.

use anamanti_core::memory::embed::{Embedder, LocalNomicEmbedder, NOMIC_FULL_DIMS};

/// (English fact, the same fact in Portuguese).
const FACTS: &[(&str, &str)] = &[
    ("My favorite color is blue", "Minha cor favorita é azul"),
    (
        "The wifi password is FastNet2024",
        "A senha do wifi é FastNet2024",
    ),
    (
        "My dentist appointment is on Tuesday at 3pm",
        "Minha consulta no dentista é na terça às 15h",
    ),
    (
        "We have a dog named Biscuit",
        "Nós temos um cachorro chamado Biscuit",
    ),
    (
        "Grandma's birthday is March 12",
        "O aniversário da vovó é 12 de março",
    ),
    ("I am allergic to peanuts", "Eu sou alérgico a amendoim"),
    (
        "The car insurance renews in June",
        "O seguro do carro renova em junho",
    ),
    (
        "The kids' school starts at 8 am",
        "A escola das crianças começa às 8 da manhã",
    ),
    (
        "I like my coffee black with no sugar",
        "Eu gosto do meu café preto sem açúcar",
    ),
    (
        "The spare key is under the blue flower pot",
        "A chave reserva está debaixo do vaso azul",
    ),
    (
        "Our anniversary is September 3",
        "Nosso aniversário de casamento é 3 de setembro",
    ),
    (
        "The plumber's name is Carlos",
        "O nome do encanador é Carlos",
    ),
];

/// (fact index, English query, Portuguese query).
const QUERIES: &[(usize, &str, &str)] = &[
    (
        0,
        "what is my favorite color",
        "qual é a minha cor favorita",
    ),
    (1, "what's the wifi password", "qual é a senha do wifi"),
    (
        2,
        "when is my dentist appointment",
        "quando é a minha consulta no dentista",
    ),
    (
        3,
        "what is our dog called",
        "como se chama o nosso cachorro",
    ),
    (
        4,
        "when is grandma's birthday",
        "quando é o aniversário da vovó",
    ),
    (5, "what am I allergic to", "eu sou alérgico a quê"),
    (
        6,
        "when does the car insurance renew",
        "quando renova o seguro do carro",
    ),
    (
        7,
        "what time does school start",
        "que horas começa a escola",
    ),
    (8, "how do I take my coffee", "como eu tomo o meu café"),
    (9, "where is the spare key", "onde está a chave reserva"),
    (
        10,
        "when is our anniversary",
        "quando é o nosso aniversário de casamento",
    ),
    (
        11,
        "what's the plumber's name",
        "qual é o nome do encanador",
    ),
];

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Rank of `target` (0 = best) among `docs` for query vector `q`.
fn rank_of(q: &[f32], docs: &[Vec<f32>], target: usize) -> usize {
    let t = cosine(q, &docs[target]);
    docs.iter().filter(|d| cosine(q, d) > t).count()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    anyhow::ensure!(
        args.len() == 3,
        "usage: bilingual_recall <model.onnx> <tokenizer dir>"
    );
    let e = LocalNomicEmbedder::open(&args[1], &args[2], NOMIC_FULL_DIMS)?;

    // The mixed store: English facts then Portuguese facts.
    let en_docs: Vec<String> = FACTS.iter().map(|f| f.0.to_string()).collect();
    let pt_docs: Vec<String> = FACTS.iter().map(|f| f.1.to_string()).collect();
    let mut store = en_docs.clone();
    store.extend(pt_docs.clone());
    let vecs = e.embed(&store).await?;
    let n = FACTS.len();

    let mut table = Vec::new();
    for (label, query_pt, target_pt) in [
        ("en query → en fact", false, false),
        ("pt query → pt fact", true, true),
        ("en query → pt fact", false, true),
        ("pt query → en fact", true, false),
    ] {
        let (mut r1, mut r3, mut ranks) = (0, 0, Vec::new());
        for (idx, qe, qp) in QUERIES {
            let q = e.embed_query(if query_pt { qp } else { qe }).await?;
            // Rank against the WHOLE mixed store (both languages compete).
            let target = if target_pt { n + idx } else { *idx };
            let r = rank_of(&q, &vecs, target);
            ranks.push(r);
            r1 += (r == 0) as usize;
            r3 += (r < 3) as usize;
        }
        table.push((label, r1, r3, ranks));
    }
    println!(
        "\nnomic-embed-text-v1.5 (local), mixed en+pt store of {} facts\n",
        store.len()
    );
    println!(
        "{:<22} {:>9} {:>9}   ranks (0 = top)",
        "direction", "recall@1", "recall@3"
    );
    for (label, r1, r3, ranks) in &table {
        println!("{label:<22} {:>6}/{n:<2} {:>6}/{n:<2}   {ranks:?}", r1, r3);
    }
    // The question that matters for "stored as spoken": is the right fact found in
    // EITHER language? (a query is answered if its fact is in the top 3 in any form)
    let mut either = 0;
    for (idx, qe, qp) in QUERIES {
        for q in [qe, qp] {
            let v = e.embed_query(q).await?;
            if rank_of(&v, &vecs, *idx) < 3 || rank_of(&v, &vecs, n + idx) < 3 {
                either += 1;
            }
        }
    }
    println!(
        "\nquery finds its fact (either language) in the top 3: {either}/{}",
        QUERIES.len() * 2
    );
    Ok(())
}
