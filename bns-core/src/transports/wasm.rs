use crate::channels::wasm::CbChannel;
use crate::encoder::Encoded;
use crate::signing::SecretKey;
use crate::signing::SigMsg;
use crate::types::channel::Channel;
use crate::types::channel::Events;
use crate::types::ice_transport::IceTransport;
use crate::types::ice_transport::IceTransportCallback;
use crate::types::ice_transport::IceTrickleScheme;
use anyhow::anyhow;
use anyhow::Result;
use async_trait::async_trait;
use log::info;
use serde::Deserialize;
use serde::Serialize;
use serde_json;
use serde_json::json;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen_futures::JsFuture;
use web_sys::MessageEvent;
use web_sys::RtcConfiguration;
use web_sys::RtcDataChannel;
use web_sys::RtcDataChannelEvent;
use web_sys::RtcIceCandidate;
use web_sys::RtcIceCandidateInit;
use web_sys::RtcIceConnectionState;
use web_sys::RtcPeerConnection;
use web_sys::RtcPeerConnectionIceEvent;
use web_sys::RtcSdpType;
use web_sys::RtcSessionDescription;
use web_sys::RtcSessionDescriptionInit;

#[derive(Clone)]
pub struct SdpString(String);

impl TryFrom<JsValue> for SdpString {
    type Error = anyhow::Error;
    fn try_from(s: JsValue) -> Result<Self> {
        match s.as_string() {
            Some(v) => Ok(Self(v)),
            None => Err(anyhow!("cannot cover {:?} to string", s)),
        }
    }
}

impl From<String> for SdpString {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<RtcSessionDescription> for SdpString {
    fn from(sdp: RtcSessionDescription) -> Self {
        Self(sdp.to_string().into())
    }
}

impl From<SdpString> for RtcSessionDescription {
    fn from(s: SdpString) -> Self {
        JsValue::from_str(&s.0).into()
    }
}

#[derive(Clone)]
pub struct WasmTransport {
    pub connection: Option<Arc<RtcPeerConnection>>,
    pub pending_candidates: Arc<Vec<RtcIceCandidate>>,
    pub channel: Option<Arc<RtcDataChannel>>,
    pub signaler: Arc<CbChannel>,
}

#[async_trait(?Send)]
impl IceTransport<CbChannel> for WasmTransport {
    type Connection = RtcPeerConnection;
    type Candidate = RtcIceCandidate;
    type Sdp = RtcSessionDescription;
    type Channel = RtcDataChannel;
    type ConnectionState = RtcIceConnectionState;
    type Msg = JsValue;

    fn new(ch: Arc<CbChannel>) -> Self {
        Self {
            connection: None,
            pending_candidates: Arc::new(vec![]),
            channel: None,
            signaler: Arc::clone(&ch),
        }
    }

    fn signaler(&self) -> Arc<CbChannel> {
        Arc::clone(&self.signaler)
    }

    async fn start(&mut self, stun: String) -> Result<()> {
        let mut config = RtcConfiguration::new();
        config.ice_servers(&JsValue::from_serde(&json! {[{"urls": stun}]}).unwrap());

        self.connection = RtcPeerConnection::new_with_configuration(&config)
            .ok()
            .as_ref()
            .map(|c| Arc::new(c.to_owned()));
        self.setup_channel("bns").await;
        return Ok(());
    }

    async fn get_peer_connection(&self) -> Option<Arc<Self::Connection>> {
        self.connection.as_ref().map(|c| Arc::clone(c))
    }

    async fn get_pending_candidates(&self) -> Vec<Self::Candidate> {
        self.pending_candidates.to_vec()
    }

    async fn get_answer(&self) -> Result<Self::Sdp> {
        match self.get_peer_connection().await {
            Some(c) => {
                let promise = c.create_answer();
                match JsFuture::from(promise).await {
                    Ok(answer) => {
                        self.set_local_description(SdpString::try_from(answer.to_owned())?)
                            .await?;
                        Ok(answer.into())
                    }
                    Err(_) => Err(anyhow!("Failed to get answer")),
                }
            }
            None => Err(anyhow!("cannot get connection")),
        }
    }

    async fn get_offer(&self) -> Result<Self::Sdp> {
        match self.get_peer_connection().await {
            Some(c) => {
                let promise = c.create_offer();
                match JsFuture::from(promise).await {
                    Ok(offer) => {
                        self.set_local_description(SdpString::try_from(offer.to_owned())?)
                            .await?;
                        Ok(offer.into())
                    }
                    Err(_) => Err(anyhow!("cannot get offer")),
                }
            }
            None => Err(anyhow!("cannot get connection")),
        }
    }

    async fn get_offer_str(&self) -> Result<String> {
        Ok(self.get_offer().await?.sdp())
    }

    async fn get_answer_str(&self) -> Result<String> {
        Ok(self.get_answer().await?.sdp())
    }

    async fn get_data_channel(&self) -> Option<Arc<Self::Channel>> {
        self.channel.as_ref().map(|c| Arc::clone(&c))
    }

    async fn set_local_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<Self::Sdp>,
    {
        match &self.get_peer_connection().await {
            Some(c) => {
                let sdp: Self::Sdp = desc.into();
                let mut offer_obj = RtcSessionDescriptionInit::new(sdp.type_());
                offer_obj.sdp(&sdp.sdp());
                let promise = c.set_local_description(&offer_obj);
                match JsFuture::from(promise).await {
                    Ok(_) => Ok(()),
                    Err(_) => Err(anyhow!("Failed to set remote description")),
                }
            }
            None => Err(anyhow!("Failed on getting connection")),
        }
    }

    async fn set_remote_description<T>(&self, desc: T) -> Result<()>
    where
        T: Into<Self::Sdp>,
    {
        match &self.get_peer_connection().await {
            Some(c) => {
                let sdp: Self::Sdp = desc.into();
                let mut offer_obj = RtcSessionDescriptionInit::new(sdp.type_());
                let sdp = &sdp.sdp();
                offer_obj.sdp(&sdp);
                let promise = c.set_remote_description(&offer_obj);

                match JsFuture::from(promise).await {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        info!("failed to set remote desc");
                        info!("{:?}", e);
                        Err(anyhow!("Failed to set remote description"))
                    }
                }
            }
            None => Err(anyhow!("Failed on getting connection")),
        }
    }

    async fn add_ice_candidate(&self, candidate: String) -> Result<()> {
        match &self.get_peer_connection().await {
            Some(c) => {
                let cand = RtcIceCandidateInit::new(&candidate);
                let promise = c.add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&cand));
                match JsFuture::from(promise).await {
                    Ok(_) => Ok(()),
                    Err(_) => {
                        log::error!("failed to add ice candate");
                        Err(anyhow!("Failed to add ice candidate"))
                    }
                }
            }
            None => Err(anyhow!("Failed on getting connection")),
        }
    }

    async fn on_ice_candidate(
        &self,
        f: Box<
            dyn FnMut(Option<Self::Candidate>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()> {
        let mut f = Some(f);
        match &self.get_peer_connection().await {
            Some(c) => {
                let callback = Closure::wrap(Box::new(move |ev: RtcPeerConnectionIceEvent| {
                    let mut f = f.take().unwrap();
                    spawn_local(async move { f(ev.candidate()).await })
                })
                    as Box<dyn FnMut(RtcPeerConnectionIceEvent)>);
                c.set_onicecandidate(Some(callback.as_ref().unchecked_ref()));
                Ok(())
            }
            None => Err(anyhow!("Failed on getting connection")),
        }
    }

    async fn on_peer_connection_state_change(
        &self,
        f: Box<
            dyn FnMut(Self::ConnectionState) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()> {
        let mut f = Some(f);
        match &self.get_peer_connection().await {
            Some(c) => {
                let callback = Closure::wrap(Box::new(move |ev: RtcIceConnectionState| {
                    let mut f = f.take().unwrap();
                    spawn_local(async move { f(ev).await })
                })
                    as Box<dyn FnMut(RtcIceConnectionState)>);
                c.set_oniceconnectionstatechange(Some(callback.as_ref().unchecked_ref()));
                Ok(())
            }
            None => Err(anyhow!("Failed on getting connection")),
        }
    }

    async fn on_data_channel(
        &self,
        f: Box<
            dyn FnMut(Arc<Self::Channel>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()> {
        let mut f = Some(f);
        match &self.get_peer_connection().await {
            Some(c) => {
                let callback = Closure::wrap(Box::new(move |ev: RtcDataChannelEvent| {
                    let mut f = f.take().unwrap();
                    spawn_local(async move { f(Arc::new(ev.channel())).await })
                })
                    as Box<dyn FnMut(RtcDataChannelEvent)>);
                c.set_ondatachannel(Some(callback.as_ref().unchecked_ref()));
                Ok(())
            }
            None => Err(anyhow!("Failed on getting connection")),
        }
    }

    async fn on_message(
        &self,
        f: Box<
            dyn FnMut(Self::Msg) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
                + Send
                + Sync,
        >,
    ) -> Result<()> {
        let mut f = Some(f);
        match &self.get_data_channel().await {
            Some(c) => {
                let callback = Closure::wrap(Box::new(move |ev: MessageEvent| {
                    let mut f = f.take().unwrap();
                    spawn_local(async move { f(ev.data()).await })
                }) as Box<dyn FnMut(MessageEvent)>);
                c.set_onmessage(Some(callback.as_ref().unchecked_ref()));
                Ok(())
            }
            None => Err(anyhow!("Failed on getting connection")),
        }
    }

    async fn on_open(
        &self,
        f: Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> + Send + Sync>,
    ) -> Result<()> {
        match &self.get_data_channel().await {
            Some(c) => {
                let callback =
                    Closure::once(Box::new(move || spawn_local(async move { f().await }))
                        as Box<dyn FnOnce()>);
                c.set_onopen(Some(callback.as_ref().unchecked_ref()));
                Ok(())
            }
            None => Err(anyhow!("Failed on getting connection")),
        }
    }
}

impl WasmTransport {
    pub async fn setup_channel(&mut self, name: &str) -> &Self {
        if let Some(conn) = &self.connection {
            let channel = conn.create_data_channel(&name);
            self.channel = Some(Arc::new(channel));
        }
        return self;
    }
}

#[async_trait(?Send)]
impl IceTransportCallback<CbChannel> for WasmTransport {
    async fn on_ice_candidate_callback(
        &self,
    ) -> Box<
        dyn FnMut(Option<Self::Candidate>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
            + Send
            + Sync,
    > {
        box move |_: Option<Self::Candidate>| Box::pin(async move {})
    }
    async fn on_peer_connection_state_change_callback(
        &self,
    ) -> Box<
        dyn FnMut(Self::ConnectionState) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
            + Send
            + Sync,
    > {
        box move |_: Self::ConnectionState| Box::pin(async move {})
    }
    async fn on_data_channel_callback(
        &self,
    ) -> Box<
        dyn FnMut(Arc<Self::Channel>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
            + Send
            + Sync,
    > {
        box move |_: Arc<Self::Channel>| Box::pin(async move {})
    }

    async fn on_message_callback(
        &self,
    ) -> Box<dyn FnMut(Self::Msg) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> + Send + Sync>
    {
        let sender = self.signaler().sender();
        box move |msg: Self::Msg| {
            let sender = Arc::clone(&sender);
            let msg = msg.as_string().unwrap().clone();
            Box::pin(async move {
                info!("{:?}", msg);
                sender.send(Events::ReceiveMsg(msg)).unwrap();
            })
        }
    }

    async fn on_open_callback(
        &self,
    ) -> Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> + Send + Sync> {
        box move || Box::pin(async move {})
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub struct TricklePayload {
    pub sdp: String,
    pub candidates: Vec<String>,
}

#[async_trait(?Send)]
impl IceTrickleScheme<CbChannel> for WasmTransport {
    // https://datatracker.ietf.org/doc/html/rfc5245
    // 1. Send (SdpOffer, IceCandidates) to remote
    // 2. Recv (SdpAnswer, IceCandidate) From Remote

    type SdpType = RtcSdpType;

    async fn get_handshake_info(&self, key: SecretKey, kind: Self::SdpType) -> Result<Encoded> {
        log::trace!("prepareing handshake info {:?}", kind);
        let sdp = match kind {
            RtcSdpType::Answer => self.get_answer().await?,
            RtcSdpType::Offer => self.get_offer().await?,
            _ => {
                return Err(anyhow!("unsupport sdp type"));
            }
        };
        let local_candidates_json: Vec<String> = self
            .get_pending_candidates()
            .await
            .iter()
            .map(|c| c.clone().to_string().into())
            .collect();
        let data = TricklePayload {
            sdp: sdp.to_string().into(),
            candidates: local_candidates_json,
        };
        log::trace!("prepared hanshake info :{:?}", data);
        let resp = SigMsg::new(data, key)?;
        Ok(resp.try_into()?)
    }

    async fn register_remote_info(&self, data: Encoded) -> anyhow::Result<()> {
        let data: SigMsg<TricklePayload> = data.try_into()?;
        log::trace!("register remote info: {:?}", data);

        match data.verify() {
            Ok(true) => {
                let sdp: SdpString = data.data.sdp.into();
                self.set_remote_description(SdpString::try_from(sdp.to_owned())?)
                    .await?;
                log::trace!("setting remote candidate");
                for c in data.data.candidates {
                    log::trace!("add candiates: {:?}", c);
                    self.add_ice_candidate(c.to_owned()).await?;
                }
                Ok(())
            }
            _ => {
                log::error!("cannot verify message sig");
                return Err(anyhow!("failed on verify message sigature"));
            }
        }
    }
}
