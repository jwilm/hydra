TODO
====

# What I was doing when I stepped away

# MVP TODOs

* Ensure streams are cleaned up when returning errors from protocol methods
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
* Remove `expect` calls which can be avoided through refactoring
