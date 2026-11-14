//! Opt-in test against an isolated running container; credentials never enter output.
use anyhow::{Context, Result};
use rivetkit::{
    TypedActorHandle,
    client::{Client, ClientConfig, GetOptions},
};
use robot_buddy::{Bot, auth::Identity, protocol::*};
use serde_json::{Value, json};

#[tokio::test]
#[ignore = "requires isolated container and BUDDY_TEST_IDENTITY_FILE"]
async fn first_boot_recovers_identity_and_scopes_the_rivet_client() -> Result<()> {
    let server = std::env::var("BUDDY_TEST_SERVER_URL")?;
    let identity: Identity =
        serde_json::from_slice(&std::fs::read(std::env::var("BUDDY_TEST_IDENTITY_FILE")?)?)?;
    let http = reqwest::Client::new();
    let mut ids = Vec::new();
    for _ in 0..2 {
        let challenge: Value = http
            .post(format!("{server}/v1/challenge"))
            .json(&identity.certificate)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let challenge = challenge["challenge"]
            .as_str()
            .context("challenge")?
            .to_owned();
        let enrollment = EnrollmentRequest {
            certificate: identity.certificate.clone(),
            proof: identity.proof(&challenge)?,
            challenge,
        };
        let response = http
            .post(format!("{server}/v1/enroll"))
            .json(&enrollment)
            .send()
            .await?;
        anyhow::ensure!(
            response.status().is_success(),
            "enrollment HTTP {}",
            response.status()
        );
        let bundle: Bundle = response.json().await?;
        assert_eq!(bundle.bot_id, identity.certificate.bot_id());
        assert!(
            !http
                .post(format!("{server}/v1/enroll"))
                .json(&enrollment)
                .send()
                .await?
                .status()
                .is_success()
        );
        ids.push((
            bundle.actor_id.clone(),
            bundle.conversation_actor_id.clone(),
            bundle.memory_actor_id.clone(),
        ));
        let client = Client::new(
            ClientConfig::new(&bundle.endpoint)
                .namespace(&bundle.namespace)
                .token(&bundle.gateway_token)
                .disable_metadata_lookup(true),
        );
        let bot = TypedActorHandle::<Bot>::new(client.get_for_id(
            "bot",
            &bundle.actor_id,
            GetOptions {
                params: Some(json!({"session":bundle.session})),
            },
        )?);
        let snapshot = bot
            .send(Call {
                session: bundle.session.clone(),
                command: Command::Snapshot,
            })
            .await?;
        assert_eq!(snapshot["bot_id"], bundle.bot_id);
        let memory = TypedActorHandle::<robot_buddy::memory::Memory>::new(client.get_for_id(
            "memory",
            &bundle.memory_actor_id,
            GetOptions {
                params: Some(json!({"session":bundle.session})),
            },
        )?);
        if std::env::var("BUDDY_TEST_EXPECT_PERSISTED").as_deref() == Ok("1") {
            let facts = memory
                .send(Call {
                    session: bundle.session.clone(),
                    command: Command::Snapshot,
                })
                .await?;
            assert!(
                facts
                    .as_array()
                    .context("facts")?
                    .iter()
                    .any(|f| f["key"] == "container-persistence" && f["value"] == "stored")
            );
        }
        memory
            .send(Call {
                session: bundle.session.clone(),
                command: Command::Remember {
                    key: "container-persistence".into(),
                    value: "stored".into(),
                },
            })
            .await?;
        let facts = memory
            .send(Call {
                session: bundle.session,
                command: Command::Snapshot,
            })
            .await?;
        assert!(
            facts
                .as_array()
                .context("facts")?
                .iter()
                .any(|f| f["key"] == "container-persistence")
        );
    }
    assert_eq!(ids[0], ids[1]);
    Ok(())
}
