# echo-proxy-core
Echo Proxy is a secure and transparent proxy based on websocket


## Usage

### Client

#### Example

```bash
echo-proxy-client --endpoint wss://example.org/proxy --user user1
```

#### More help information
```
Usage: echo-proxy-client [OPTIONS] --endpoint <ENDPOINT> --user <USER>

Options:
      --endpoint <ENDPOINT>  Remote server address
      --user <USER>          User id
      --host <HOST>          Local server address [default: 127.0.0.1]
      --port <PORT>          Local server port [default: 9002]
  -h, --help                 Print help
  -V, --version              Print version
```

### Server
Config web server to echo proxy server.

```conf
# example nginx conf
server {
        # For security reasons, it is strongly recommended to enable SSL.
        listen                      443 ssl http2;
        ssl_certificate             /root/cert/cert.pem;
        ssl_certificate_key         /root/cert/key.pem;
        server_name                 www.example.org;
        root                        /data/www.example.org;
        index                       index.html index.htm index.nginx-debian.html;

        location /proxy/ {
                proxy_redirect off;
                proxy_pass http://127.0.0.1:9001;
                proxy_http_version 1.1;
                proxy_set_header Upgrade $http_upgrade;
                proxy_set_header Connection "upgrade";
                proxy_set_header Host $http_host;

                proxy_set_header X-Real-IP $remote_addr;
                proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        }
}

```



#### Example

```bash
echo-proxy-server --user user1 --user user2
```


#### More help information
```
Usage: echo-proxy-server [OPTIONS]

Options:
      --users <USERS>  User id list
      --host <HOST>    Local server address [default: 127.0.0.1]
      --port <PORT>    Local server port [default: 9001]
  -h, --help           Print help
  -V, --version        Print version
```


