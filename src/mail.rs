use anyhow::Result;
use chrono::Local;
use lettre::message::{Attachment, MultiPart, SinglePart, header::ContentType};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use tokio::runtime::Handle;

use crate::config::EmailConfig;

#[derive(Clone)]
pub struct Mailer {
    config: EmailConfig,
}

impl Mailer {
    pub fn new(config: EmailConfig) -> Self {
        Self { config }
    }

    /// Déclenche l'envoi de l'alerte e-mail avec l'image JPEG de la détection
    pub fn send_alert(&self, image_bytes: Vec<u8>) {
        let config = self.config.clone();

        if !config.enabled {
            return;
        }

        // Exécution asynchrone pour ne pas bloquer le flux vidéo de la caméra
        if let Ok(handle) = Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = Self::dispatch_email(config, image_bytes).await {
                    eprintln!("❌ Erreur lors de l'envoi de l'email : {}", e);
                }
            });
        }
    }

    async fn dispatch_email(config: EmailConfig, image_bytes: Vec<u8>) -> Result<()> {
        let now = Local::now().format("%d/%m/%Y à %H:%M:%S").to_string();

        // 1. Template HTML Responsive
        let html_template = format!(
            r#"<!DOCTYPE html>
<html lang="fr">
<head>
    <meta charset="UTF-8">
    <style>
        body {{
            font-family: 'Segoe UI', Tahoma, Geneva, Verdana, sans-serif;
            background-color: #121212;
            color: #e0e0e0;
            margin: 0;
            padding: 20px;
        }}
        .container {{
            max-width: 600px;
            margin: 0 auto;
            background-color: #1e1e1e;
            border-radius: 12px;
            overflow: hidden;
            box-shadow: 0 4px 20px rgba(0,0,0,0.5);
            border: 1px solid #333333;
        }}
        .header {{
            background-color: #d32f2f;
            color: #ffffff;
            padding: 20px;
            text-align: center;
        }}
        .header h1 {{
            margin: 0;
            font-size: 22px;
            letter-spacing: 1px;
        }}
        .content {{
            padding: 25px;
        }}
        .alert-info {{
            background-color: #2a2a2a;
            border-left: 4px solid #f44336;
            padding: 12px 16px;
            margin-bottom: 20px;
            border-radius: 4px;
            color: #ffffff; 
        }}
        .alert-info p {{
            color: #ffffff; 
        }}
        .photo-box {{
            text-align: center;
            margin-top: 15px;
        }}
        .photo-box img {{
            max-width: 100%;
            height: auto;
            border-radius: 8px;
            border: 2px solid #d32f2f;
            box-shadow: 0 2px 10px rgba(211, 47, 47, 0.3);
        }}
        .footer {{
            background-color: #141414;
            text-align: center;
            padding: 15px;
            font-size: 12px;
            color: #777777;
            border-top: 1px solid #2a2a2a;
        }}
    </style>
</head>
<body>
    <div class="container">
        <div class="header">
            <h1>🦊 FOXGUARD SECURITY</h1>
        </div>
        <div class="content">
            <div class="alert-info">
                <strong>🚨 Alerte d'intrusion détectée !</strong>
                <p style="margin: 5px 0 0 0; font-size: 14px;">Horodatage : {now}</p>
            </div>
            <p>Un objet ou un individu suspect a été capturé par le système de détection IA :</p>
            <div class="photo-box">
                <img src="cid:detection_photo" alt="Capture de la détection" />
            </div>
        </div>
        <div class="footer">
            Notification automatique générée par FoxGuard Surveillance System.
        </div>
    </div>
</body>
</html>"#
        );

        // 2. Pièce jointe Image avec Content-ID (CID)
        let image_part = Attachment::new_inline("detection_photo".to_string())
            .body(image_bytes, ContentType::parse("image/jpeg")?);

        // 3. Construction du message MIME multipart
        let email = Message::builder()
            .from(config.from_address.parse()?)
            .to(config.to_address.parse()?)
            .subject("🚨 Alerte FoxGuard : Détection d'intrusion !")
            .multipart(
                MultiPart::related()
                    .singlepart(SinglePart::html(html_template))
                    .singlepart(image_part),
            )?;

        let creds = Credentials::new(config.smtp_user.clone(), config.smtp_password.clone());

        let mailer: AsyncSmtpTransport<Tokio1Executor> =
            AsyncSmtpTransport::<Tokio1Executor>::relay(&config.smtp_server)?
                .credentials(creds)
                .build();

        mailer.send(email).await?;
        println!("📧 E-mail d'alerte avec photo HTML envoyé avec succès !");

        Ok(())
    }
}
