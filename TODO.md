TODO
====

# What I was doing when I stepped away

Just made HTTP requests successfully! Added a reregister function that may be
super suboptimal when handling many notifications.

# MVP TODOs

* Resolve mpsc::Sender vs mio::Sender use for worker / event loop messages
    * Fuck it, everything can just use the mio::Sender.
* stream errors send RST_STREAM. I assume this already happens internally with
  the protocol, but if reading data for an outgoing stream payload fails, that
  would need to happen manually.
* Writing streams. The StreamHandle has a get_data_chunk method that returns
  data as long as it doesn't say it's done. Keep an active list of stream ids
  that we still need to write data for.
* DNS lookup for non blocking TcpStreams. Sync at first; just use
  to_socket_addrs. Maybe just spawn a thread and send a TcpStream back to the
  event loop after connect has been called.
* TLS connections
* Shutdown cleanup so everything can finish as much as possible.

# Additional TODOs

- Async DNS lookup the proper way
