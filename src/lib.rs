//! A library for running a USB/IP server

use futures::stream::StreamExt;
use log::*;
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;
use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::io::Result;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

pub mod cdc;
mod consts;
mod device;
mod endpoint;
pub mod hid;
mod interface;
mod setup;
mod util;
pub use consts::*;
pub use device::*;
pub use endpoint::*;
pub use interface::*;
pub use setup::*;
pub use util::*;

/// Main struct of a USB/IP server
pub struct UsbIpServer {
    pub devices: Vec<UsbDevice>,
}

async fn handler<T: AsyncReadExt + AsyncWriteExt + Unpin>(
    mut socket: &mut T,
    server: Arc<UsbIpServer>,
) -> Result<()> {
    let mut current_import_device = None;
    loop {
        let mut command = [0u8; 4];
        socket.read_exact(&mut command).await?;
        match command {
            [0x01, 0x11, 0x80, 0x05] => {
                debug!("Got OP_REQ_DEVLIST");
                let _status = socket.read_u32().await?;

                // OP_REP_DEVLIST
                socket.write_u32(0x01110005).await?;
                socket.write_u32(0).await?;
                socket.write_u32(server.devices.len() as u32).await?;
                for dev in &server.devices {
                    dev.write_dev_with_interfaces(&mut socket).await?;
                }
                debug!("Sent OP_REP_DEVLIST");
            }
            [0x01, 0x11, 0x80, 0x03] => {
                debug!("Got OP_REQ_IMPORT");
                let _status = socket.read_u32().await?;
                let mut bus_id = [0u8; 32];
                socket.read_exact(&mut bus_id).await?;
                current_import_device = None;
                for device in &server.devices {
                    let mut expected = device.bus_id.as_bytes().to_vec();
                    expected.resize(32, 0);
                    if expected == bus_id {
                        current_import_device = Some(device);
                        info!("Found device {:?}", device.path);
                        break;
                    }
                }

                // OP_REP_IMPORT
                debug!("Sent OP_REP_IMPORT");
                socket.write_u32(0x01110003).await?;
                if let Some(dev) = current_import_device {
                    socket.write_u32(0).await?;
                    dev.write_dev(&mut socket).await?;
                } else {
                    socket.write_u32(1).await?;
                }
            }
            [0x00, 0x00, 0x00, 0x01] => {
                debug!("Got USBIP_CMD_SUBMIT");
                let seq_num = socket.read_u32().await?;
                let dev_id = socket.read_u32().await?;
                let direction = socket.read_u32().await?;
                let ep = socket.read_u32().await?;
                let transfer_flags = socket.read_u32().await?;
                let transfer_buffer_length = socket.read_u32().await?;
                let start_frame = socket.read_u32().await?;
                let number_of_packets = socket.read_u32().await?;
                let interval = socket.read_u32().await?;
                let mut setup = [0u8; 8];
                socket.read_exact(&mut setup).await?;
                let device = current_import_device.unwrap();
                let real_ep = if direction == 0 { ep } else { ep | 0x80 };
                let (usb_ep, intf) = device.find_ep(real_ep as u8).unwrap();
                debug!("->Endpoint {:02x?}", usb_ep);
                debug!("->Setup {:02x?}", setup);
                let resp = device
                    .handle_urb(socket, usb_ep, intf, transfer_buffer_length, setup)
                    .await?;
                debug!("<-Resp {:02x?}", resp);

                // USBIP_RET_USBMIT
                // command
                socket.write_u32(0x3).await?;
                socket.write_u32(seq_num).await?;
                socket.write_u32(dev_id).await?;
                socket.write_u32(direction).await?;
                socket.write_u32(ep).await?;
                // status
                socket.write_u32(0).await?;
                // actual length
                socket.write_u32(resp.len() as u32).await?;
                // start frame
                socket.write_u32(0).await?;
                // number of packets
                socket.write_u32(0).await?;
                // error count
                socket.write_u32(0).await?;
                // setup
                socket.write_all(&setup).await?;
                // data
                socket.write_all(&resp).await?;
            }
            [0x00, 0x00, 0x00, 0x02] => {
                debug!("Got USBIP_CMD_UNLINK");
            }
            _ => warn!("Got unknown command {:?}", command),
        }
    }
}

/// Spawn a USB/IP server at `addr` using tokio
pub async fn server(addr: SocketAddr, server: UsbIpServer) {
    let mut listener = TcpListener::bind(addr).await.expect("bind to addr");

    let server = async move {
        let usbip_server = Arc::new(server);
        let mut incoming = listener.incoming();
        while let Some(socket_res) = incoming.next().await {
            match socket_res {
                Ok(mut socket) => {
                    info!("Got connection from {:?}", socket.peer_addr());
                    let new_server = usbip_server.clone();
                    tokio::spawn(async move {
                        let res = handler(&mut socket, new_server).await;
                        info!("Handler ended with {:?}", res);
                    });
                }
                Err(err) => {
                    warn!("Got error {:?}", err);
                }
            }
        }
    };

    server.await
}

#[cfg(test)]
mod test {
    use super::*;
    use std::{
        io::*,
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::{AsyncRead, AsyncWrite};

    pub struct MockSocket {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl MockSocket {
        pub fn new(input: Vec<u8>) -> Self {
            Self {
                input: Cursor::new(input),
                output: vec![],
            }
        }
    }

    impl AsyncRead for MockSocket {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context,
            buf: &mut [u8],
        ) -> Poll<Result<usize>> {
            Poll::Ready(std::io::Read::read(&mut self.get_mut().input, buf))
        }
    }

    impl AsyncWrite for MockSocket {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize>> {
            self.get_mut().output.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn req_devlist() {
        let intf_handler = Arc::new(Mutex::new(
            Box::new(cdc::UsbCdcAcmHandler::new()) as Box<dyn UsbInterfaceHandler + Send>
        ));
        let server = UsbIpServer {
            devices: vec![UsbDevice::new(0).with_interface(
                ClassCode::CDC as u8,
                cdc::CDC_ACM_SUBCLASS,
                0x00,
                "Test CDC ACM",
                cdc::UsbCdcAcmHandler::endpoints(),
                intf_handler.clone(),
            )],
        };

        // OP_REQ_DEVLIST
        let mut mock_socket = MockSocket::new(vec![0x01, 0x00, 0x80, 0x05, 0x00, 0x00, 0x00, 0x00]);
        handler(&mut mock_socket, Arc::new(server)).await.ok();
        println!("{:?}", mock_socket.output);
    }
}
