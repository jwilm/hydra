TODO
====

# What I was doing when I stepped away

About to write a HydraPrioritizer/StatefulPrioritizer etc which tracks which
streams have an interest. Tracking write interest is critical for continuing to
get writable events from the loop. Let's also utilize the tick function to
buffer stream outgoing data so that it can be written immediately at the next
write.

# MVP TODOs

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

# Nice to haves

* Write a macro for building a type, implementing StreamHandler, and running
  tests given a spec defined in the macro invocation. Sadly, I think this
  probably will require procedural macros or a compiler extension. Maybe that's
  ok since it's just for testing. Could use syntex to do the codegen.

# Version 1.0

* Async DNS lookup the proper way
* Optimize event loop reregistration
