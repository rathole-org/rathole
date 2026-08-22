# Out of Scope

`rathole` focuses on the forwarding for the NAT traversal, rather than being an all-in-one development tool or a load balancer or a gateway. It's designed to *be used with them*, not *replace them*.

But that doesn't mean it's not useful for other purposes. In the future, more configuration APIs will be added and `rathole` can be used with an external dashboard.

> Make each program do one thing well.

- *Domain based forwarding for HTTP*

  Introducing these kinds of features into `rathole` itself ultimately reinvents an nginx. Use nginx to do this and set `rathole` as the upstream. This method achieves better performance as well as flexibility.

- *HTTP Request Logging*

  `rathole` doesn't interfere with the application layer traffic. A right place for this kind of stuff is the web server, and a network capture tool.

- *`frp`'s STCP or other setup that requires visitors' side configuration*

  If that kind of setup is possible, then there are a lot more tools available. You may want to consider secure tunnels like WireGuard or ZeroTier. `rathole` primarily focuses on NAT traversal by forwarding, which doesn't require any setup for visitors. 

- *Caching `local_ip`'s DNS records*

  As responded in [issue #183](https://github.com/rapiz1/rathole/issues/183), `local_ip` cache is not feasible because we have no reliable way to detect ip change. Handling DNS TTL and so on should be done with a DNS server, not a client. Caching ip is generally dangerous for clients. If you care about the `local_ip` query you can set up a local DNS server and enable caching. Then the local lookup should be trivial.
