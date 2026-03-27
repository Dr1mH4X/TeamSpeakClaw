import React from "react";
import clsx from "clsx";
import Link from "@docusaurus/Link";
import Layout from "@theme/Layout";
import Heading from "@theme/Heading";
import styles from "./index.module.css";
import Translate, { translate } from "@docusaurus/Translate";

function HomepageHeader() {
  return (
    <header className={styles.heroBanner}>
      <div className="container" style={{ position: "relative", zIndex: 1 }}>
        <Heading as="h1" className={styles.hero__title}>
          TeamSpeakClaw
        </Heading>
        <p className={styles.hero__subtitle}>
          <Translate id="homepage.tagline">下一代 TeamSpeak LLM 集成</Translate>
        </p>
        <div className={styles.buttons}>
          <Link
            className={clsx("button button--lg", styles.getStartedButton)}
            to="/docs/intro"
          >
            <Translate id="homepage.getStarted">快速开始</Translate>
          </Link>
        </div>
      </div>
    </header>
  );
}

export default function Home(): React.JSX.Element {
  return (
    <Layout
      title={translate({ id: "homepage.title", message: "首页" })}
      description={translate({
        id: "homepage.description",
        message: "极简 TS3 机器人",
      })}
    >
      <HomepageHeader />
      <main className={styles.featuresSection}>
        <div className="container">
          <div className="row">
            <FeatureItem
              title={
                <Translate id="homepage.features.naturalInteraction.title">
                  自然交互
                </Translate>
              }
              description={
                <Translate id="homepage.features.naturalInteraction.description">
                  使用自然语言控制您的服务器。无需记忆指令。
                </Translate>
              }
            />
            <FeatureItem
              title={
                <Translate id="homepage.features.rbacSecurity.title">
                  RBAC 安全机制
                </Translate>
              }
              description={
                <Translate id="homepage.features.rbacSecurity.description">
                  基于角色的访问控制系统。安全、细粒度且强大。
                </Translate>
              }
            />
            <FeatureItem
              title={
                <Translate id="homepage.features.cloudMusic.title">
                  云音乐集成
                </Translate>
              }
              description={
                <Translate id="homepage.features.cloudMusic.description">
                  无缝集成网易云音乐，打造高品质频道。
                </Translate>
              }
            />
          </div>
        </div>
      </main>
    </Layout>
  );
}

function FeatureItem({ title, description }) {
  return (
    <div className="col col--4">
      <div className={styles.featureCard}>
        <Heading as="h3" className={styles.featureCardTitle}>
          {title}
        </Heading>
        <p className={styles.featureCardDesc}>{description}</p>
      </div>
    </div>
  );
}
