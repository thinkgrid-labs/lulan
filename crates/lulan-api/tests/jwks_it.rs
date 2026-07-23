//! The JWKS identity adapter, against a stub IdP.
//!
//! One adapter is meant to cover Auth0, Clerk, Keycloak, Supabase,
//! Firebase and anything else publishing a JWK Set, so what matters is
//! that it accepts a correctly signed token and refuses every near-miss.
//! No network and no IdP account: a local axum server plays the provider.

use std::net::SocketAddr;

use axum::Router;
use axum::routing::get;
use jsonwebtoken::{EncodingKey, Header};
use lulan_api::identity::{IdentityProvider, JwksIdentity};
use serde_json::json;

const KID: &str = "test-key-1";
const ISSUER: &str = "https://idp.jwks-test";

const PRIVATE_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCc/jR/pcbM9OW3\nsY5ZDc73ObEoFUL+0cCXTcrDg7ecnH/cnlRWV/QokBOSI6r8ZnqW1BwRDj46uffW\nG2QcJlWrnjiL2BoX1tKEr1ph1X6F66o/taPxdOeX8dkEWx1i+05XvcAGqYvvNKzf\nvrfSgQcD/1s6jimS6UeudN0gAHVSfYHod2SJ7U8sfW+qIabG8bvcBbe/sGA42upl\nbu+CqejmQ1eGY0sDmY9L1DPmLZEjUBjjYALbP8oKEfKvQld81JJFwFrPcCrX0IwS\nAs4gLFjYn0stanfaIDmzg5D07Bi5zB3nqUTv0ReWr/YLPC6CSUIKm/TX+FDATelp\n71gECYgfAgMBAAECggEAN28XUH9TzCkpOAytC8rxaqBnapTfXCTqUUK5twG6gsVL\n7LXHZ9mUsAH3tyF7DbaN0NZCts8FsCzcUzDGz53FoFy08MQ0qnhDS3CzhFojW9xT\n+D0GD4tM/3z5OS2HGd1M03R/6ppRe/xoknTAacb/mCzBpNJv4Z0Xn4VKUzN2OdLj\nkt9GN+g13DKRaXQIU5cRE6C1k1YgUbm52JW7N9KiNCQQnhVv5cHaU62zi8HvgHiA\niGzI/7QIguM0PXy+nySECdQtjP7ia2tKhKWQY6OHO9v+ZjyyLCH0o3P0w1Ysc5gF\nPGRCL2VMPwhIe6FD6zrVHXV400BEvk3feBEc9P5gYQKBgQDTx2JZ26Q9wHnTu1sp\n0cKE/e6mmkhLHZr3pk0/hrrFiE6ah5Pzg3hN8Eb3Ke3kKTs19AOdUR1UnntgALoJ\nN4uWFEYUSAJEZIOO8auXurmJBjvJFvoArxGe1aCRKetrJDl4bifEKO1LnEF/UWbf\nJ8OnDbYg8W3VBBIoZdSqINv1EQKBgQC9xj+tESLfFR+ike7V/m2tc3tnFHvr6TDi\nvE3igD6iQfnGQMw+ldk8mxsrCoiWJnQliGL2kKlD7U/Cf3Bl8iXke3ZHF18J28Sq\nM581JNrS7RdiKIw6ZzP88YFWBdhsbZOFr+rbZQZLQtSZZakb1zBz4Nj4RwoDY8rD\nToFDO0bqLwKBgBQoYEYpT+LI1U+//5dlbdx2xyZ4fPUZZky4OZYYXuK6bLDswrpl\nAyh3/Gk+RnR3MDmcdlOdCuupAhlLOGn0LYclet4nVH/qCVOr0SdqEIroWvxzAWzb\nPRQfRV9L3Cqgkg/SfFqBgsS1pM5XkzEeedMGzRUppcim0Iuj1bAz3HvhAoGBAJHd\nQHJA2qHHNbKaIo5+6kRIoBBB8WIJsdaE0ASJeBr1RQu6IIL2YKwxt/ckOInYcquq\nog5McJ3SWNzxYS4qqi9tKiNIdnc4YXhFB1ksw7keHTwIWIhHbPE9m6DIC2qD6sD1\nzznk86qDaq+hMRNCGm7m4z4qNCsY1++4dqh7dm1nAoGBAJFPe6pTUafLVe6vsdvA\nprlLLusxfGlXByrFThm7EgdI3K55TnskYOZVxOlIwTCdnowqcAbWQMAPPR0rQtOK\njodbS33B7h91ZsCMPbBu97lIco8aL/hBkgwVvqQ0I+CzY7WPP+Jlh7zbA3Owkwar\nSBAC4CJXwQpL5wbxS6oi45gv\n-----END PRIVATE KEY-----";
const MODULUS: &str = "nP40f6XGzPTlt7GOWQ3O9zmxKBVC_tHAl03Kw4O3nJx_3J5UVlf0KJATkiOq_GZ6ltQcEQ4-Orn31htkHCZVq544i9gaF9bShK9aYdV-heuqP7Wj8XTnl_HZBFsdYvtOV73ABqmL7zSs37630oEHA_9bOo4pkulHrnTdIAB1Un2B6Hdkie1PLH1vqiGmxvG73AW3v7BgONrqZW7vgqno5kNXhmNLA5mPS9Qz5i2RI1AY42AC2z_KChHyr0JXfNSSRcBaz3Aq19CMEgLOICxY2J9LLWp32iA5s4OQ9OwYucwd56lE79EXlq_2CzwugklCCpv01_hQwE3pae9YBAmIHw";
const OTHER_PRIVATE_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDKk2+WRDoMGumi\nuMdi0nUk1nuhxuOvcVM4msVnWA7hMW/f8r3lKwY4/KhoIekto2NPCyDPLT7TYorD\nWvy+Ie08qFRAuqSOft7p6fEUON2STeOn/maFKg8FLSjlxLTlszuW8Hn2hPwioxmD\n+WPCZUfQjhnOWpuKtOhnrhIaQAHAbTvJDHo0jU1CtfsJQfFx1ScBNpGuGvpLLnRQ\nOR1iFj3f2G+46fGt7qTvGW7pAVrenXgnPICHTPi7SiClmq7bL1+yMkIWm9D95bp4\nG6RWEKGqhSqyhiY6ETRB5pXh/y/TVkodyHHPso52Fc4f88qk1IDa/D5P852QSU/E\n2/0l265bAgMBAAECggEACvqysZJBEybiQs+A/vopHzYAvWPqMzgRKqA40sETDWGA\njsK5SBJs4+nNttcfrDmOyFnyDUiGfy54Ft53lFaCZSc6acez44U8z7C+QDpUx/kw\nvYosREtjRQSNkuZ2Z3yvXh7qfVH32Gi+mUiK/549pjANdgGzLHFhpzzn/kQ51BP+\n2Zc0GnvhRewJecfWiRyoY3T5S9YS+IQNuts1hjM3iRMsBo9gyTn3KhgxHpRs6LG6\nx0ztkuN5+zR94TTRjnFQjC0bDz91MJiJWgA09ppzNz8Xg9W8WN+7D8ehMCxk7drk\nlhqqynV8q9+OfucqtgVXuV+zFME/xZa97Oy0Ep/vAQKBgQDmqw9Tmlr8vRDOBPec\nKvA90xd3BaHmcRthkUaGzfbTTTgX+vVq7sLZ/cXon7qKKaUK1MinGQ3uMTp/BGv5\nida5YPkrvkw8SWnNhZFbln7AD55lH4JKzqmK5U1qXPlkmlOG5P1XjG3X3glKy/bN\n/KWG4SGuYqfeFh2+PbY3xwrN7QKBgQDg0pj+Hs9y/J0lrbWNtoUXTEsocAOKum7g\nFfFoWdXHF57SbZDTY0wNiG+LkF9bSvp7qYlob1Le0INx8I44iswJq1aMOrlZ4+1e\n7T0Y+mxFzqCEyWlSS2Y5iofNjUTZeZmmUjdvOgXt2OW2LYasI5EoMdK+4pr9QTI7\n7ipPzBOkZwKBgQC4qL6XTh2C8RRv6XgUFCfJRqElTmQCqA+kdvl/14i+NbYvNF+d\n4FAq1UbHaH+cNaSDXD7ZzmvhgJV0s6SA20EDnMc8ppY/OQIzXrc0G/GSba5/A895\ndaIyqEjmWlHooMc3WUAbAze4NW846rnEw3n71WTyRtZeK1RaROsIEhbrLQKBgEQ+\n6X5KcAKhuDpVzsTj4Oa/nBj8V7bm/P086/kXPBOhke6in9HrVIzPG70r6CZYTkz3\nm+R91pQYi64srZ9wUpukzTLoKJem3slwDpnkerV+Ea/9S+FVTgStjqfQ+FNj3EZm\nsrkqzd3zd1ej3jum2EtxRF35f77c6ZjTpThv5I6FAoGBALT/sT8+5lO/ndXIqbg1\nx5eRv3G5ezi/xTzZDCIypNwk6RK/ec+eSXS8c1huOMoRO6JHnxNZKYOlalrOmFUG\nXoHEetxn35CtIJKYS9RnTGhtJnZv6ab/7kWF7Ue46QIEuhLrFmyf13icKRn/wD+4\n7wNJGN+dVtT978+imMH4LBiP\n-----END PRIVATE KEY-----";
const EXPONENT: &str = "AQAB";

async fn stub_idp(keys: serde_json::Value) -> SocketAddr {
    let app = Router::new().route(
        "/jwks.json",
        get(move || {
            let keys = keys.clone();
            async move { axum::Json(keys) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn jwk_set() -> serde_json::Value {
    json!({"keys": [{
        "kty": "RSA", "use": "sig", "alg": "RS256", "kid": KID,
        "n": MODULUS, "e": EXPONENT,
    }]})
}

fn token(claims: serde_json::Value, kid: Option<&str>) -> String {
    let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = kid.map(str::to_string);
    jsonwebtoken::encode(
        &header,
        &claims,
        &EncodingKey::from_rsa_pem(PRIVATE_PEM.as_bytes()).unwrap(),
    )
    .unwrap()
}

fn valid_claims() -> serde_json::Value {
    json!({
        "iss": ISSUER,
        "sub": "user-42",
        "email": "traveller@example.com",
        "aud": "lulan",
        "exp": chrono::Utc::now().timestamp() + 3600,
    })
}

#[tokio::test]
async fn jwks_provider_accepts_real_tokens_and_refuses_near_misses() {
    let addr = stub_idp(jwk_set()).await;
    let provider =
        JwksIdentity::connect(ISSUER.to_string(), format!("http://{addr}/jwks.json"), None)
            .await
            .expect("the stub publishes a usable key set");

    // The happy path: claims come back intact for the customer upsert.
    let subject = provider
        .verify(&token(valid_claims(), Some(KID)))
        .expect("a correctly signed token verifies");
    assert_eq!(subject.issuer, ISSUER);
    assert_eq!(subject.subject, "user-42");
    assert_eq!(subject.email.as_deref(), Some("traveller@example.com"));

    // Every near-miss must fail closed.
    let mut expired = valid_claims();
    expired["exp"] = json!(chrono::Utc::now().timestamp() - 3600);
    let mut wrong_issuer = valid_claims();
    wrong_issuer["iss"] = json!("https://attacker.example");

    for (label, token) in [
        ("expired", token(expired, Some(KID))),
        ("issued by someone else", token(wrong_issuer, Some(KID))),
        (
            "kid not in the published set",
            token(valid_claims(), Some("unknown-kid")),
        ),
        ("no kid at all", token(valid_claims(), None)),
        ("not a JWT", "garbage".to_string()),
    ] {
        assert!(
            provider.verify(&token).is_none(),
            "{label} must not authenticate"
        );
    }

    // Signed with a real but unpublished key, under a kid that IS
    // published. The signature is perfectly well-formed — which is
    // exactly why it must be checked against the right key. This was
    // previously gated behind an env var that was never set, so it
    // silently never ran.
    let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(KID.to_string());
    let forged = jsonwebtoken::encode(
        &header,
        &valid_claims(),
        &EncodingKey::from_rsa_pem(OTHER_PRIVATE_PEM.as_bytes()).unwrap(),
    )
    .unwrap();
    assert!(
        provider.verify(&forged).is_none(),
        "a token signed with a key the IdP never published must not authenticate"
    );
}

#[tokio::test]
async fn an_empty_or_unreachable_key_set_fails_at_boot() {
    // Better to refuse to start than to accept nothing later and look
    // like every customer's token is invalid.
    let addr = stub_idp(json!({"keys": []})).await;
    assert!(
        JwksIdentity::connect(ISSUER.into(), format!("http://{addr}/jwks.json"), None)
            .await
            .is_err(),
        "an empty key set is a misconfiguration, not a valid state"
    );
    assert!(
        JwksIdentity::connect(ISSUER.into(), "http://127.0.0.1:1/jwks.json".into(), None)
            .await
            .is_err(),
        "an unreachable JWKS URL must fail at boot"
    );
}
