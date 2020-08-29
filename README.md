# layline
A utility to tunnel network connections over HTTP.

## installing

Requires rust >= 1.45.

```
git clone https://github.com/morfca/layline.git
cd layline
cargo build --release
```

## description

This utility provides a server and client in a combined binary. The server may be exposed directly to the internet, however in typical usage it is expected it will operate behind a more full-featured HTTP server, for example, Apache with mod_proxy. This utility does not provide https, name-based virtual hosting, or other features that are likely to be useful in a real world installation. The server only terminates connections to the IP/port provided on the command line as a security measure, to prevent it from being abused to proxy connections to arbitrary hosts.

There are two client modes. The first creates a standing process that listens for connections and creates new sessions to tunnel each connection. The second is intended for use with processes that can read/write to stdin/stdout, for example with the "ProxyCommand" option in OpenSSH. Please note that unlike ssh, layline **does not provide any encryption of its own**. It is the responsibility of the user to ensure their configuration is either using https or that the tunneled protocol provides encryption and verification by itself (for example, ssh). Note that even with encryption, the tunneled session is likely to be discoverable with statistical/behavioral monitoring.

## motivation

tl;dr provide tunneled connections through an HTTP host, using a subset of HTTP likely to be supported by almost any proxy/CDN/load balancer, and with the HTTP host presenting on the network as a typical HTTP host using common server software. This is achieved by:

1. avoiding the SSH host listening directly on any externally accessible port, as the SSH server identification string is easily detected in network scans
2. avoiding the use of protocol multiplexors such as sslh; these may still be detected by network scans and this may not work behind proxies/load balancers
3. supporting arbitrary HTTP paths; this allows the HTTP root and other paths to serve other functions
4. avoiding the use of the CONNECT method, this may be unsupported by some proxies or loadbalancers
5. using only the GET POST and DELETE methods
6. using small body sizes (<1 MiB) and short lived requests (<10s) for maximum compatibility with proxies and loadbalancers, including cloud load balancers and CDNs
7. due to network filtering and throttling, plaintext HTTP is supported as an option

This utility can, for example, provide ssh access to a bastion host over HTTP proxied via CDN using a shared CDN IP address.

## usage

### server

```layline server HTTP_LISTEN:PORT TUNNEL_DEST:PORT```

### client

```layline client TUNNEL_DEST:PORT https://example.org/layline/```

### proxyclient

```layline proxyclient https://example.org/layline/```

### options

--log-dest may be used to specify a directory for logging in server mode. The default is to log to stderr

--max-sessions may be used to specify the maximum number of sessions in either client or server mode. The default is 100. This option is meaningless in proxyclient mode because new connections are not listened for.

--session-timeout may be used to specify the inactivity timeout in client or server mode. The default is 900 seconds. Repeatedly calling the /recv endpoint when there is no incoming data or calling the /send endpoint with a zero length body do not count as activity.

--allow-plaintext is used to override the default which is to refuse plaintext HTTP URLs

## example configurations

### apache

this hosts layline API endpoints at the example path /abc123/ on a plaintext host that redirects all other paths to the https host, with the layline server running on 127.0.0.1:8080.

```
<VirtualHost *:80>
  ServerName www.example.org
  RedirectMatch "^/(?!abc123/).*" "https://www.example.org$0"
  ProxyPass "/abc123/" "http://localhost:8080/"
</VirtualHost>
```

### openssh

this can be added to a user's .ssh/config file to automatically tunnel via HTTPS for a particular hostname

```
Host example.org
  ProxyCommand layline proxyclient https://example.org/abc123/
```

with this config in place an ssh session can be established transparently

```
user@laptop ~ $ ssh example.org
Last login: Mon April 20 16:20 2020 from 1.2.3.4
user@server ~ $
```

## API

### GET /create

Returns a session ID in the HTTP body. This session must be provided in the X-Layline-Session header in further requests.

### GET /recv

Returns bytes from the tunneled connection, up to 1 MiB. If no bytes are available to read, wait for up to 10s before returning with zero bytes in the HTTP body. May be called again immediately after returning regardless of body size.

### POST /send

Sends bytes on the tunneled connection, up to 1 MiB.

### DELETE /close

Destroys the session on the server. This will cause the connection to be disconnected. There are no guarantees that bytes available to /recv will be available after this is called, or that bytes sent to /send will have been sent to the downstream connection.

### remote disconnect

In some cases the remote side of the session may disconnect asynchronously. This may happen if the server side idle timeout is reached, the remote connection on the server side is terminated, the layline process is restarted, etc. In such cases subsequent requests to the /recv, /send, or /close endpoints will return a 404. The client treats these cases as the connection being cleanly terminated by the remote side, and will close the associated session locally as well. In proxyclient mode the client process will terminate. The session will terminate with error with all other HTTP status codes or failure to connect, for example layer 7 load balancers and proxies typically return a 503 when the backend service is unavailable. HTTP client retries are not implemented at this time.
