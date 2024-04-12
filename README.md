# Forward

一个 Rust 编写的 HTTP 内网穿透工具。

## 用法

需要一台公网服务器和一个 HTTPS 证书。

假设域名 `a.foo.com`, `b.foo.com` 指向公网服务器，
要把对 `a.foo.com:4430`的访问转发到本机 8080 端口，
对 `b.foo.com:4430`的访问转发到本机 8081 端口。

在服务器上执行：

```bash
./forward server --client 0.0.0.0:4431 --http 0.0.0.0:4430 \
  --cert 证书 --key 证书私钥 --secret 密钥 
```

在本机执行：

```bash
./forward client --server a.foo.com:4431 --secret 密钥 \
  --forward a.foo.com:127.0.0.1:8080 --forward b.foo.com:127.0.0.1:8081
```

（密钥可以是任意字符串，服务端客户端保持一致就行。）