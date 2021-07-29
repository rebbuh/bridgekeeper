# Bridgekeeper

> "What... is your favorite policy language?"
>
> "Rego. (shocked) No... Pythoooooon!!!"
>
> -- Based loosely on Monty Python and the Holy Grail

Bridgekeeper helps you to enforce policies in your kubernetes cluster by providing a simple declarative way to define constraints using the python programming language. Whenever resources are created or updated matching constraints will be evaluated and if a constraint is violated the resource will be rejected.

From a technical perspective bridgekeeper acts as a kubernetes [admission webhook](https://kubernetes.io/docs/reference/access-authn-authz/extensible-admission-controllers/). For every admission request that it gets it will check all registered constraints if any match the resource of the request and if yes the constraint will be evaluated and based on the result the admission request will be allowed or rejected.

Bridgekeeper is very similar to and heavily inspired by [OPA gatekeeper](https://github.com/open-policy-agent/gatekeeper). It was born out of the idea to make gatekeeper simpler and use python instead of rego (because we love python) as the policy language.

**This service is work-in-progress and should not yet be used in production setups. Use at your own risk.**

## User Guide

### Installation Requirements

* A kubernetes cluster (version >= 1.19) with cluster-admin permissions
* Helm

### Installation

If you want to use a released version:

1. Add the helm repo: `helm repo add bridgekeeper https://maibornwolff.github.io/bridgekeeper/`
2. Install the chart: `helm install --namespace bridgekeeper --create-namespace bridgekeeper bridgekeeper/bridgekeeper`

If you want to use the current master version from git:

1. Install the chart: `helm install --namespace bridgekeeper --create-namespace bridgekeeper ./charts/bridgekeeper`

### Writing constraints

Bridgekeeper uses a custom resource called Constraint to manage policies. A constraint consists of a target that describes what kubernetes resources are to be validated by the constraint and a rule script written in python.

Below is a minimal example:

```yaml
apiVersion: bridgekeeper.maibornwolff.de/v1alpha1
kind: Constraint
metadata:
  name: foobar
spec:
  target:
    matches:
      - apiGroup: apps
        kind: "Deployment"
  rule:
    python: |
      def validate(request):
        return False, "You don't want to deploy anything today"
```

The constraint has the following fields:

* `target.matches`: A list of one or more match parameters consisting of `apiGroup` and `kind`. Wildcards can be used as `"*"`, if the resource has no API group (e.g. namespaces) use an empty string `""`.
* `target.namespaces`: An optional list of strings, if specified only resources from one of these namespaces are matched, only this or `target.excludedNamespaces` can be specified, not both
* `target.excludedNamespaces`: An optional list of strings, if specified resources from one of these namespaces are not matched, only this or `target.namespaces` can be specified, not both
* `rule.python`: An inline python script. It must have a function called `validate` that gets exactly one parameter that contains the [AdmissionRequest](https://github.com/kubernetes/api/blob/master/admission/v1/types.go#L40)) as a python structure as a `json.loads` would produce it. The function must return either a boolean or a tuple of boolean and string. The boolean represents the result (true means resource is accepted, false means rejected) and the string is an optional reason for the rejection that will be sent to the caller.

You can use the entire python feature set and standard library in your script (so stuff like `import re` is possible). Using threads, accessing the filesystem or using the network (e.g. via sockets) should not be done and might be prohibited in the future.

You can find a more useful example under [example/constraint.yaml](example/constraint.yaml) that denies deployments that use a docker image with a `latest` tag. Try it out using the following steps:

1. `kubectl apply -f example/constraint.yaml` to create the constraint
2. `kubectl apply -f example/deployment-error.yaml` will yield an error because the docker image is referenced using the `latest` tag
3. `kubectl apply -f example/deployment-ok.yaml` will be accepted because the docker image tag uses a specific version

Constraints are not namespaced and apply to the entire cluster unless `target.namespaces` or `target.excludedNamespaces` are used to filter namespaces. To completely exclude a namespace from bridgekeeper label it with `bridgekeeper/ignore`. If you use the helm chart to install bridgekeeper you can set the option `bridgekeeper.ignoreNamespaces` to a list of namespaces that should be labled and it will be done during initial install (by default it will label the `kube-*` namespaces).

## Developer Guide

This service is written in Rust and uses [kube-rs](https://github.com/clux/kube-rs) as kubernetes client, [rocket](https://rocket.rs/) as web framework and [PyO3](https://pyo3.rs/) as python bindings.

### Requirements

* Current stable Rust (version >=1.53) with cargo
* Python >= 3.8 with shared library (pyenv by default does not provide a shared library, install with `PYTHON_CONFIGURE_OPTS="--enable-shared" pyenv install <version>` to enable it)
* A kubernetes cluster (version >= 1.19) with cluster-admin permissions, this guide assumes a local [k3s](https://k3s.io/) cluster set up with [k3d](https://k3d.io/)
* kubectl, helm
* Tested only on linux, should also work on MacOS

### Start service locally

1. Compile binary: `cargo build`
2. Generate certificates and install webhook: `cargo run -- init --local host.k3d.internal:8081`
3. Install CRD: `kubectl apply -f charts/bridgekeeper/crds/constraint.yaml`
4. Launch bridgekeeper: `cargo run -- server --cert-dir .certs`

After you are finished, run `cargo run -- cleanup --local` to delete the webook.

### Development cycle

As long as you do not change the schema of the constraints you can just recompile and restart the server without having to reinstall any of the other stuff (certificate, webhook, constraints).

### Test cluster deployment

1. Build docker image: `docker build . -t bridgekeeper:dev`
2. Upload docker image into cluster: `k3d image import bridgekeeper:dev`
3. Deploy helm chart: `helm upgrade --install --namespace bridgekeeper --create-namespace bridgekeeper ./charts/bridgekeeper --set image.repository=bridgekeeper --set image.tag=dev`

## Planned features

* Give rules access to existing objects of the same type (to do e.g. uniqueness checks)
* Check existing resources against constraints (audit)
* Ability to modify/patch resources
