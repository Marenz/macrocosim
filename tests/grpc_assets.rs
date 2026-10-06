//! gRPC integration tests for the assets API surface
//! (`frequenz.api.assets.v1.PlatformAssets`). Each test spawns a
//! fresh `TestServer` and exercises the assets RPCs against the
//! same topology the microgrid tests use.

mod common;

use common::TestServer;
use macrocosim::proto::assets::{
    GetMicrogridRequest, ListMicrogridElectricalComponentConnectionsRequest,
    ListMicrogridElectricalComponentsRequest, platform_assets_client::PlatformAssetsClient,
};

const TINY_TOPOLOGY: &str = r#"
(make-microgrid :id 7 :grpc-port 8800 :topology (lambda ()
(%make-grid-connection-point :id 1
            :successors
            (list (%make-meter :id 2
                               :successors
                               (list (%make-battery-inverter
                                      :id 4
                                      :rated-lower-w -5000.0
                                      :rated-upper-w  5000.0
                                      :successors
                                      (list (%make-battery
                                             :id 3
                                             :rated-lower-w -5000.0
                                             :rated-upper-w  5000.0)))))))
))
"#;

async fn connect(s: &TestServer) -> PlatformAssetsClient<tonic::transport::Channel> {
    PlatformAssetsClient::connect(s.assets_url.clone())
        .await
        .expect("grpc connect")
}

#[tokio::test(flavor = "multi_thread")]
async fn get_microgrid_returns_configured_id() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let resp = c
        .get_microgrid(GetMicrogridRequest { microgrid_id: 7 })
        .await
        .expect("get ok")
        .into_inner();
    let m = resp.microgrid.expect("microgrid present");
    assert_eq!(m.id, 7);
}

#[tokio::test(flavor = "multi_thread")]
async fn get_microgrid_rejects_wrong_id() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let err = c
        .get_microgrid(GetMicrogridRequest { microgrid_id: 999 })
        .await
        .expect_err("wrong id should be rejected");
    assert_eq!(err.code(), tonic::Code::NotFound, "got {err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_components_returns_topology() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let resp = c
        .list_microgrid_electrical_components(ListMicrogridElectricalComponentsRequest {
            microgrid_id: 7,
            ..Default::default()
        })
        .await
        .expect("list ok")
        .into_inner();
    assert_eq!(resp.microgrid_id, 7);
    let ids: Vec<u64> = resp.components.iter().map(|c| c.id).collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&2));
    assert!(ids.contains(&3));
    assert!(ids.contains(&4));
}

#[tokio::test(flavor = "multi_thread")]
async fn list_components_filters_by_id() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let resp = c
        .list_microgrid_electrical_components(ListMicrogridElectricalComponentsRequest {
            microgrid_id: 7,
            component_ids: vec![2, 3],
            ..Default::default()
        })
        .await
        .expect("list ok")
        .into_inner();
    let mut ids: Vec<u64> = resp.components.iter().map(|c| c.id).collect();
    ids.sort();
    assert_eq!(ids, vec![2, 3]);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_components_rejects_wrong_id() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let err = c
        .list_microgrid_electrical_components(ListMicrogridElectricalComponentsRequest {
            microgrid_id: 999,
            ..Default::default()
        })
        .await
        .expect_err("wrong id should be rejected");
    assert_eq!(err.code(), tonic::Code::NotFound, "got {err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_connections_returns_edges() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let resp = c
        .list_microgrid_electrical_component_connections(
            ListMicrogridElectricalComponentConnectionsRequest {
                microgrid_id: 7,
                ..Default::default()
            },
        )
        .await
        .expect("list ok")
        .into_inner();
    assert_eq!(resp.microgrid_id, 7);
    // grid → meter, meter → inverter, inverter → battery.
    assert_eq!(resp.connections.len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_connections_rejects_wrong_id() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let err = c
        .list_microgrid_electrical_component_connections(
            ListMicrogridElectricalComponentConnectionsRequest {
                microgrid_id: 999,
                ..Default::default()
            },
        )
        .await
        .expect_err("wrong id should be rejected");
    assert_eq!(err.code(), tonic::Code::NotFound, "got {err:?}");
}
